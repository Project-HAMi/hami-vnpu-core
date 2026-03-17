#include <stdio.h>
#include <stdlib.h>
#include <unistd.h> 
#include <acl/acl.h>

#define CHUNK_SIZE (256ULL * 1024ULL * 1024ULL)
#define MAX_CHUNKS 1024

void print_mem_info(const char* label) {
    size_t free, total;
    if (aclrtGetMemInfo(ACL_HBM_MEM, &free, &total) == ACL_ERROR_NONE) {
        printf("\n[%s]\n", label);
        printf("  System Total: %10.2f GB\n", (double)total / (1024*1024*1024));
        printf("  System Free:  %10.2f GB\n", (double)free / (1024*1024*1024));
        printf("  System Used:  %10.2f GB\n", (double)(total - free) / (1024*1024*1024));
    }
}

void stressTest256MB(int deviceId) {
    void* ptrs[MAX_CHUNKS];
    int count = 0;
    aclError ret;

    aclInit(NULL);
    aclrtSetDevice(deviceId);

    print_mem_info("BEFORE ALLOCATION");
    printf("\nStarting Demo: Allocating 256MB chunks ...\n");
    // printf("Check npu-smi in another terminal to see real-time growth!\n\n");
    // printf("chunk size is {:}\n", CHUNK_SIZE);

    while (count < MAX_CHUNKS) {
        void* devicePtr = NULL;
        ret = aclrtMalloc(&devicePtr, CHUNK_SIZE, ACL_MEM_MALLOC_HUGE_FIRST);

        if (ret != ACL_ERROR_NONE) {
            printf("\n[!!!] OOM TRIGGERED at Chunk #%d\n", count + 1);
            printf("      Error Code: %d\n", ret);
            break; 
        }

        aclrtMemset(devicePtr, CHUNK_SIZE, 0, CHUNK_SIZE);

        ptrs[count] = devicePtr;
        count++;

        printf("\rAllocated: [%3d] | Total: %6.2f GB", count, (double)count * 256.0 / 1024.0);
        fflush(stdout); 

        usleep(100000); 
    }

    printf("\n\n------------------------------------------\n");
    printf("DEMO RESULT SUMMARY:\n");
    printf("  Target Limit (Quota):   ??? GB (Check your env)\n");
    printf("  Actually Captured:      %.2f GB\n", (double)count * 256.0 / 1024.0);
    printf("------------------------------------------\n");

    print_mem_info("AFTER ALLOCATION (PEAK)");

    printf("\nCleaning up resources...\n");
    for (int i = 0; i < count; i++) {
        aclrtFree(ptrs[i]);
    }

    aclrtResetDevice(deviceId);
    aclFinalize();
    printf("Test finished successfully.\n");
}

int main() {
    stressTest256MB(0);
    return 0;
}