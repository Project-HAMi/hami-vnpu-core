#!/usr/bin/env python3
"""Validate a Linux release library against the installed CANN SDK, without an NPU.

Run inside the native build container with the same library search path used for
compilation. This checks linkage and SDK API availability; it does not initialize
CANN/DCMI or claim hardware compatibility.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys


RUNTIME_HOOKS = {
    "rtAicpuKernelLaunchExWithArgs", "rtAicpuKernelLaunchWithFlag",
    "rtKernelLaunchWithFlagV2", "rtKernelLaunchWithHandleV2", "rtModelExecute",
    "rtMalloc", "rtMallocCached", "rtMemAlloc", "rtMallocPhysical",
    "rtFreePhysical", "rtsMemMallocPhysical", "rtFree", "rtsFree", "rtMemGetInfoEx",
    "rtDvppMalloc", "rtDvppMallocWithFlag", "rtDvppFree", "rtMemRetainAllocationHandle",
    "rtSetDevice", "rtCtxSetCurrent", "rtStreamCreate", "rtStreamCreateWithFlags",
    "rtStreamCreateWithConfig", "rtStreamDestroy", "rtStreamDestroyForce",
    "rtCtxCreate", "rtCtxDestroy", "rtDeviceReset", "rtDeviceResetEx",
    "rtDeviceResetWithoutTsd", "rtDeviceResetForce",
}
RUNTIME_LINK_APIS = {
    "rtStreamCreate", "rtEventCreate", "rtEventRecord", "rtStreamWaitEvent",
    "rtStreamSynchronize", "rtEventElapsedTime", "rtSetDevice", "rtGetDevice",
    "rtDeviceSynchronize", "rtStreamGetCaptureInfo", "rtGetDevicePhyIdByIndex",
    "rtCtxGetCurrent",
}
ACL_HOOKS = {
    "aclrtSetDevice", "aclrtSetCurrentContext", "aclrtResetDevice",
    "aclrtResetDeviceForce", "aclrtSetDeviceResLimit", "aclrtResetDeviceResLimit",
    "aclrtSetStreamResLimit", "aclrtResetStreamResLimit",
    "aclrtCreateStream", "aclrtCreateStreamWithConfig", "aclrtCreateStreamV2", "aclrtDestroyStream",
    "aclrtDestroyStreamForce", "aclrtCreateContext", "aclrtDestroyContext",
}
SPATIAL_APIS = ACL_HOOKS | {
    "aclrtGetDevice", "aclrtGetDeviceResLimit", "aclrtGetStreamResLimit",
}
# Some SDK headers declare optional creation variants that the corresponding
# runtime does not export. Their interceptors are only used when native callers
# use that variant; they are not hard-linked requirements of libvnpu.
OPTIONAL_RUNTIME_APIS = {"rtStreamCreateWithConfig"}
MACHINES = {"x86_64": "Advanced Micro Devices X86-64", "aarch64": "AArch64"}


def run(*command, env=None):
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, env=env, check=False)
    if result.returncode:
        raise ValueError("{} failed:\n{}".format(" ".join(command), result.stdout))
    return result.stdout


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def exports(path, env):
    names = set()
    for line in run("readelf", "--dyn-syms", "--wide", str(path), env=env).splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[4] in {"GLOBAL", "WEAK"} and fields[6] != "UND":
            names.add(fields[7].split("@", 1)[0])
    return names


def require_symbols(path, expected, env):
    missing = expected - exports(path, env)
    if missing:
        raise ValueError("{} lacks required exports: {}".format(path, ", ".join(sorted(missing))))


def cann_metadata(toolkit, arch, expected_version, explicit_file):
    candidates = [explicit_file] if explicit_file else [
        toolkit / (arch + "-linux") / "ascend_toolkit_install.info",
        *sorted(toolkit.glob("share/info/*/version.info")),
        toolkit / "version.cfg",
        toolkit / "version.info",
        Path("/etc/Ascend/ascend_cann_install.info"),
    ]
    for candidate in candidates:
        if not candidate.is_file():
            continue
        content = candidate.read_text(encoding="utf-8")
        # Match the complete version token, rather than a longer version prefix.
        if not expected_version or re.search(
                r"(?<![\w.])" + re.escape(expected_version) + r"(?![\w.])", content):
            return candidate.resolve(strict=True), content
    if expected_version or explicit_file:
        raise ValueError("CANN {} metadata not found; inspected: {}".format(
            expected_version or "version", ", ".join(str(path) for path in candidates)))
    return None, None


def validate(args):
    library = args.library.resolve(strict=True)
    toolkit = args.toolkit_home.resolve(strict=True)
    driver = args.driver_home.resolve(strict=True)
    runtime = (toolkit / "lib64/libruntime.so").resolve(strict=True)
    acl = (toolkit / "lib64/libascendcl.so").resolve(strict=True)
    dcmi = (driver / "lib64/driver/libdcmi.so").resolve(strict=True)
    version_file, version_text = cann_metadata(
        toolkit, args.arch, args.cann_version, args.cann_version_file)
    driver_version_file = driver / "version.info"
    driver_metadata = None
    if driver_version_file.is_file():
        driver_metadata = {
            "version_file": str(driver_version_file.resolve(strict=True)),
            "version_file_sha256": sha256(driver_version_file),
            "version_metadata": driver_version_file.read_text(encoding="utf-8"),
        }

    env = dict(os.environ, LC_ALL="C")
    env.pop("LD_PRELOAD", None)
    env.pop("LD_AUDIT", None)
    inherited_paths = [path for path in env.get("LD_LIBRARY_PATH", "").split(":")
                       if path and not {"devlib", "stub", "stubs"}.intersection(Path(path).parts)]
    env["LD_LIBRARY_PATH"] = ":".join(dict.fromkeys([
        str(toolkit / "lib64"), str(driver / "lib64/driver"),
        str(driver / "lib64/common"), *inherited_paths,
    ]))
    header = run("readelf", "--file-header", "--wide", str(library), env=env)
    if not re.search(r"Class:\s+ELF64\b", header):
        raise ValueError("release library is not ELF64")
    if not re.search(r"Type:\s+DYN\b", header):
        raise ValueError("release library is not an ELF shared object")
    if not re.search(r"Machine:\s+" + re.escape(MACHINES[args.arch]) + r"\s*$", header, re.M):
        raise ValueError("release library does not match architecture " + args.arch)

    dynamic = run("readelf", "--dynamic", "--wide", str(library), env=env)
    needed = re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic)
    missing = {"libc.so.6"} - set(needed)
    if missing:
        raise ValueError("missing real SDK/system dependencies: " + ", ".join(sorted(missing)))
    sonames = re.findall(r"\(SONAME\).*?\[(.*?)\]", dynamic)
    if sonames and sonames != ["libvnpu.so"]:
        raise ValueError("unexpected SONAME: " + ", ".join(sonames))
    # Ordinary processes must start even when CANN/driver libraries are absent.
    eager = {"libruntime.so", "libascendcl.so", "libdcmi.so"}.intersection(needed)
    if eager:
        raise ValueError("unexpected eager SDK dependencies: " + ", ".join(sorted(eager)))
    require_symbols(library, RUNTIME_HOOKS | ACL_HOOKS, env)
    require_symbols(runtime, RUNTIME_LINK_APIS | {
        "rtCtxSetCurrent", "rtStreamCreateWithFlags",
        "rtStreamDestroy", "rtStreamDestroyForce", "rtCtxCreate", "rtCtxDestroy",
        "rtDeviceReset", "rtDeviceResetEx", "rtDeviceResetWithoutTsd", "rtDeviceResetForce",
    }, env)
    optional_runtime_missing = sorted(OPTIONAL_RUNTIME_APIS - exports(runtime, env))
    missing_spatial = sorted(SPATIAL_APIS - exports(acl, env))
    if args.require_spatial_apis and missing_spatial:
        raise ValueError("CANN lacks spatial APIs: " + ", ".join(missing_spatial))
    require_symbols(dcmi, {"dcmi_init", "dcmi_get_card_list", "dcmi_get_device_resource_info",
                           "dcmi_get_card_id_device_id_from_phyid"}, env)

    linkage = run("ldd", "-r", str(library), env=env)
    if re.search(r"not found|undefined symbol|symbol lookup error", linkage, re.I):
        raise ValueError("unresolved release dependencies:\n" + linkage)
    return {
        "schema_version": 1,
        "architecture": args.arch,
        "library": {"name": library.name, "sha256": sha256(library),
                    "size_bytes": library.stat().st_size, "soname": sonames[0] if sonames else None,
                    "needed": needed, "verified_hook_exports": sorted(RUNTIME_HOOKS | ACL_HOOKS)},
        "cann": {"version": args.cann_version, "sdk_image": args.sdk_image,
                 "version_file": str(version_file) if version_file else None,
                 "version_file_sha256": sha256(version_file) if version_file else None,
                 "version_metadata": version_text,
                 "spatial_api_available": not missing_spatial,
                 "missing_spatial_apis": missing_spatial,
                 "optional_runtime_apis_missing": optional_runtime_missing},
        "driver": driver_metadata,
        "sdk_libraries": {name: {"path": str(path), "sha256": sha256(path)}
                          for name, path in {"runtime": runtime, "acl": acl, "dcmi": dcmi}.items()},
        "toolchain": {"rustc": run("rustc", "--version", env=env).strip(),
                      "cargo": run("cargo", "--version", env=env).strip()},
        "source_commit": args.source_commit or run("git", "rev-parse", "HEAD", env=env).strip(),
        "dynamic_linkage": linkage,
        "validation": "ELF, SDK exports and dynamic relocations; lazy CANN/DCMI resolution; no NPU initialized",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", type=Path)
    parser.add_argument("--arch", choices=MACHINES, required=True)
    parser.add_argument("--cann-version", help="Expected version; requires matching metadata")
    parser.add_argument("--cann-version-file", type=Path)
    parser.add_argument("--sdk-image", help="Immutable reference of the SDK build image")
    parser.add_argument("--source-commit", default=os.environ.get("SOURCE_COMMIT"),
                        help="Source commit when the build context omits .git")
    parser.add_argument("--require-spatial-apis", action="store_true",
                        help="Also require the optional spatial-policy CANN APIs")
    parser.add_argument("--toolkit-home", type=Path,
                        default=Path(os.environ.get("ASCEND_TOOLKIT_HOME", "/usr/local/Ascend/ascend-toolkit/latest")))
    parser.add_argument("--driver-home", type=Path,
                        default=Path(os.environ.get("ASCEND_DRIVER_HOME", "/usr/local/Ascend/driver")))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        report = validate(args)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    except (OSError, ValueError) as error:
        print("release validation failed: {}".format(error), file=sys.stderr)
        return 1
    print("Validated {} ({}, CANN {}), SHA256 {}".format(
        args.library, args.arch, args.cann_version or "version recorded in build metadata",
        report["library"]["sha256"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
