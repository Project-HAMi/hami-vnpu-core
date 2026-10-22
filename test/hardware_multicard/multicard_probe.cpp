#include "acl/acl.h"
#include "aclnnop/aclnn_mm.h"

#include <dlfcn.h>
#include <unistd.h>
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

namespace {
constexpr int64_t kSide = 512;
constexpr size_t kElements = kSide * kSide;
constexpr size_t kBytes = kElements * sizeof(uint16_t);

std::string json(const std::string &value) {
    std::string out = "\"";
    for (unsigned char ch : value) {
        if (ch == '\\' || ch == '"') { out += '\\'; out += ch; }
        else if (ch >= 0x20) { out += ch; }
        else { char escaped[7]; std::snprintf(escaped, sizeof(escaped), "\\u%04x", ch); out += escaped; }
    }
    return out + "\"";
}

void checked(long long code, const char *operation) {
    if (code == 0) return;
    const char *recent = aclGetRecentErrMsg();
    throw std::runtime_error(std::string(operation) + " failed: " + std::to_string(code)
        + (recent ? std::string("; ") + recent : ""));
}

unsigned integer(const char *value, const char *name) {
    if (!value || !*value) throw std::runtime_error(std::string("missing ") + name);
    char *end = nullptr;
    const unsigned long result = std::strtoul(value, &end, 10);
    if (*end || result > 100000) throw std::runtime_error(std::string("invalid ") + name);
    return static_cast<unsigned>(result);
}

std::string provider(const char *symbol) {
    Dl_info info{};
    void *address = dlsym(RTLD_DEFAULT, symbol);
    if (!address || !dladdr(address, &info) || !info.dli_fname) {
        throw std::runtime_error(std::string("cannot find provider for ") + symbol);
    }
    return info.dli_fname;
}

float half_value(uint16_t value) {
    const float sign = (value & 0x8000) ? -1.0f : 1.0f;
    const unsigned exponent = (value >> 10) & 31;
    const unsigned mantissa = value & 1023;
    if (exponent == 31) return mantissa ? NAN : sign * INFINITY;
    if (exponent == 0) return sign * std::ldexp(float(mantissa), -24);
    return sign * std::ldexp(1.0f + float(mantissa) / 1024.0f, int(exponent) - 15);
}

struct Resources {


    int device = 0;
    aclrtStream stream = nullptr;
    void *a = nullptr, *b = nullptr, *c = nullptr, *workspace = nullptr;
    aclTensor *ta = nullptr, *tb = nullptr, *tc = nullptr;
    void close(bool verify) {
        long long first_error = 0;
        auto record = [&](long long rc) { if (rc && !first_error) first_error = rc; };
        if (ta) { record(aclDestroyTensor(ta)); ta = nullptr; }
        if (tb) { record(aclDestroyTensor(tb)); tb = nullptr; }
        if (tc) { record(aclDestroyTensor(tc)); tc = nullptr; }
        if (workspace) { record(aclrtFree(workspace)); workspace = nullptr; }
        if (a) { record(aclrtFree(a)); a = nullptr; }
        if (b) { record(aclrtFree(b)); b = nullptr; }
        if (c) { record(aclrtFree(c)); c = nullptr; }
        if (stream) { record(aclrtDestroyStream(stream)); stream = nullptr; }


        if (verify) checked(first_error, "resource cleanup");
    }
    ~Resources() { close(false); }
};

aclTensor *tensor(void *address) {
    const int64_t shape[] = {kSide, kSide};
    const int64_t strides[] = {kSide, 1};
    aclTensor *result = aclCreateTensor(shape, 2, ACL_FLOAT16, strides, 0,
        ACL_FORMAT_ND, shape, 2, address);
    if (!result) throw std::runtime_error("aclCreateTensor returned null");
    return result;
}
} // namespace

void matmul(Resources &r) {
        checked(aclrtMalloc(&r.a, kBytes, ACL_MEM_MALLOC_HUGE_FIRST), "aclrtMalloc(A)");
        checked(aclrtMalloc(&r.b, kBytes, ACL_MEM_MALLOC_HUGE_FIRST), "aclrtMalloc(B)");
        checked(aclrtMalloc(&r.c, kBytes, ACL_MEM_MALLOC_HUGE_FIRST), "aclrtMalloc(C)");
        const std::vector<uint16_t> ones(kElements, 0x3c00); // IEEE FP16 1.0
        std::vector<uint16_t> result(kElements, 0xffff);
        checked(aclrtMemcpy(r.a, kBytes, ones.data(), kBytes, ACL_MEMCPY_HOST_TO_DEVICE), "aclrtMemcpy(A)");
        checked(aclrtMemcpy(r.b, kBytes, ones.data(), kBytes, ACL_MEMCPY_HOST_TO_DEVICE), "aclrtMemcpy(B)");
        r.ta = tensor(r.a); r.tb = tensor(r.b); r.tc = tensor(r.c);
        uint64_t workspace_size = 0;
        aclOpExecutor *executor = nullptr;
        // Device selection and spatial budget setup precede ACLNN tiling.
        checked(aclnnMmGetWorkspaceSize(r.ta, r.tb, r.tc, 0, &workspace_size, &executor), "aclnnMmGetWorkspaceSize");
        if (workspace_size) checked(aclrtMalloc(&r.workspace, workspace_size, ACL_MEM_MALLOC_HUGE_FIRST), "aclrtMalloc(workspace)");
        checked(aclnnMm(r.workspace, workspace_size, executor, r.stream), "aclnnMm");
        checked(aclrtSynchronizeStream(r.stream), "aclrtSynchronizeStream");
        checked(aclrtMemcpy(result.data(), kBytes, r.c, kBytes, ACL_MEMCPY_DEVICE_TO_HOST), "aclrtMemcpy(result)");
        size_t mismatches = 0;
        float max_error = 0;
        for (uint16_t value : result) {
            if (value != 0x6000) ++mismatches; // IEEE FP16 512.0
            const float error = std::fabs(half_value(value) - float(kSide));
            max_error = std::isfinite(error) ? std::max(max_error, error) : INFINITY;
        }
        if (mismatches) throw std::runtime_error("MatMul mismatch: " + std::to_string(mismatches) + " elements");

    std::printf("{\"event\":\"matmul\",\"device\":%d,\"verified_elements\":%zu,\"mismatches\":%zu}\n", r.device, kElements, mismatches);
}

int main(int argc, char **argv) {
    try {
        if (argc != 2) throw std::runtime_error("usage: multicard_probe native|spatial|dynamic|fixed|overdraft");
        const std::string mode = argv[1];
        const bool managed = mode != "native";
        const bool spatial = mode == "spatial";
        if (managed && mode != "spatial" && mode != "dynamic" && mode != "fixed" && mode != "overdraft") throw std::runtime_error("unknown policy");
        const unsigned percent = managed ? integer(std::getenv("NPU_COMPUTE_LIMIT"), "NPU_COMPUTE_LIMIT") : 100;
        const unsigned quota_mb = managed ? integer(std::getenv("NPU_MEMORY_LIMIT_MB"), "NPU_MEMORY_LIMIT_MB") : 512;
        if (quota_mb != 512) throw std::runtime_error("smoke requires a 512 MiB per-card quota");
        if (managed != (provider("aclrtSetDevice").find("libvnpu") != std::string::npos)) throw std::runtime_error("unexpected hook provider");
        checked(aclInit(nullptr), "aclInit");
        uint32_t count = 0;
        checked(aclrtGetDeviceCount(&count), "device count");
        if (count != 2) throw std::runtime_error("expose exactly two idle cards");
        std::this_thread::sleep_for(std::chrono::milliseconds(3200));
        Resources r[2];
        void *large[2] = {nullptr, nullptr};
        void *extra = nullptr;
        int32_t physical[2] = {-1, -1};
        for (int i=0; i<2; ++i) {
            r[i].device=i;
            checked(aclrtSetDevice(i), "select device");
            checked(aclrtGetPhyDevIdByLogicDevId(i, &physical[i]), "physical mapping");
            checked(aclrtCreateStream(&r[i].stream), "create stream");
        }
        if (physical[0] != 6 || physical[1] != 7) throw std::runtime_error("expected physical cards 6 and 7");
        for (int i=0; i<2; ++i) {
            checked(aclrtSetDevice(i), "reselect device");
            const unsigned cube = spatial ? (20*percent+99)/100 : 20;
            const unsigned vector = spatial ? (40*percent+99)/100 : 40;
            if (spatial) {
                checked(aclrtSetStreamResLimit(r[i].stream, ACL_RT_DEV_RES_CUBE_CORE, 20), "set stream Cube after both cards initialized");
                checked(aclrtSetStreamResLimit(r[i].stream, ACL_RT_DEV_RES_VECTOR_CORE, 40), "set stream Vector after both cards initialized");
                checked(aclrtResetStreamResLimit(r[i].stream), "reset stream budget");
            }
            uint32_t read_cube=0, read_vector=0;
            checked(aclrtGetStreamResLimit(r[i].stream, ACL_RT_DEV_RES_CUBE_CORE, &read_cube), "read stream Cube");
            checked(aclrtGetStreamResLimit(r[i].stream, ACL_RT_DEV_RES_VECTOR_CORE, &read_vector), "read stream Vector");
            if (read_cube != cube || read_vector != vector) throw std::runtime_error("stream budget mismatch");
            checked(aclrtMalloc(&large[i], 300ULL*1024*1024, ACL_MEM_MALLOC_NORMAL_ONLY), "300 MiB on each card");
            size_t free_bytes=0,total_bytes=0;
            checked(aclrtGetMemInfo(ACL_HBM_MEM, &free_bytes, &total_bytes), "per-card memory report");
            if (managed && total_bytes != 512ULL*1024*1024) throw std::runtime_error("per-card quota report mismatch");
            std::printf("{\"event\":\"card\",\"device\":%d,\"physical\":%d,\"cube\":%u,\"vector\":%u,\"total_bytes\":%zu}\n", i,physical[i],read_cube,read_vector,total_bytes);
        }
        if (managed) {
            checked(aclrtSetDevice(0), "select card for rejection");
            void *denied=nullptr;
            const aclError rc=aclrtMalloc(&denied,300ULL*1024*1024,ACL_MEM_MALLOC_NORMAL_ONLY);
            if (rc == 0) { aclrtFree(denied); throw std::runtime_error("card 0 exceeded its own quota"); }
            checked(aclrtSetDevice(1), "select independent card");
            checked(aclrtMalloc(&extra,64ULL*1024*1024,ACL_MEM_MALLOC_NORMAL_ONLY), "card 1 unaffected by card 0 rejection");
            checked(aclrtFree(extra), "free extra"); extra=nullptr;
            std::printf("{\"event\":\"quota\",\"aggregate_allocated_mib\":600,\"per_card_limit_mib\":512,\"over_limit_rejected\":true,\"other_card_unaffected\":true}\n");
        }
        if (managed) {
            size_t before0=0,before1=0,after=0,total=0;
            checked(aclrtSetDevice(1), "VMM target select");
            checked(aclrtGetMemInfo(ACL_HBM_MEM,&before1,&total), "VMM target before");
            checked(aclrtSetDevice(0), "VMM current select");
            checked(aclrtGetMemInfo(ACL_HBM_MEM,&before0,&total), "VMM current before");
            aclrtPhysicalMemProp prop{};
            prop.handleType=ACL_MEM_HANDLE_TYPE_NONE;
            prop.allocationType=ACL_MEM_ALLOCATION_TYPE_PINNED;
            prop.memAttr=ACL_HBM_MEM_HUGE;
            prop.location.id=1;
            prop.location.type=ACL_MEM_LOCATION_TYPE_DEVICE;
            aclrtDrvMemHandle handle=nullptr;
            checked(aclrtMallocPhysical(&handle,2*1024*1024,&prop,0), "VMM target card 1 while current 0");
            int32_t current=-1;
            checked(aclrtGetDevice(&current), "VMM current unchanged");
            if (current != 0) throw std::runtime_error("VMM hook changed current device");
            checked(aclrtGetMemInfo(ACL_HBM_MEM,&after,&total), "VMM current after");
            if (after != before0) throw std::runtime_error("VMM charged current card instead of target");
            checked(aclrtSetDevice(1), "VMM target after select");
            checked(aclrtGetMemInfo(ACL_HBM_MEM,&after,&total), "VMM target after");
            if (before1 != after+2*1024*1024) throw std::runtime_error("VMM target charge mismatch");
            checked(aclrtSetDevice(0), "VMM cross-card free select");
            checked(aclrtFreePhysical(handle), "VMM cross-card free");
            checked(aclrtSetDevice(1), "VMM refund select");
            checked(aclrtGetMemInfo(ACL_HBM_MEM,&after,&total), "VMM target refunded");
            if (after != before1) throw std::runtime_error("VMM target refund mismatch");
            std::printf("{\"event\":\"physical_memory\",\"current_card\":0,\"target_card\":1,\"bytes\":2097152,\"target_accounting_verified\":true}\n");
        }
        for (int i=0; i<2; ++i) {
            checked(aclrtSetDevice(i), "select for MatMul");
            matmul(r[i]);
            checked(aclrtFree(large[i]), "free large allocation"); large[i]=nullptr;
            r[i].close(true);
        }
        for (int i=0; i<2; ++i) checked(aclrtResetDevice(i), "reset device");
        checked(aclFinalize(), "aclFinalize");
        std::printf("{\"status\":\"pass\",\"mode\":%s,\"physical_cards\":[6,7],\"verified_elements\":%zu}\n", json(mode).c_str(),2*kElements);
        return 0;
    } catch (const std::exception &error) {
        std::fprintf(stderr, "{\"status\":\"fail\",\"error\":%s}\n", json(error.what()).c_str());
        return 1;
    }
}
