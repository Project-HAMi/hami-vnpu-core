# HostArgs compute admission regression

Run on Linux with Rust and a C compiler:

```sh
bash test/host_args.sh
```

The fixture builds the actual interceptor against fake CANN libraries. It checks
the public CANN `aclrtLaunchKernelWithHostArgs` ABI (eight arguments, including
64-bit sizes, and a negative `aclError`), then reads the real worker's shared
token counter. It exercises ACL loaded with both `RTLD_GLOBAL` and `RTLD_LOCAL`.

- Eager calls consume one token and wait when no token is available.
- Capture records no execution, leaves the token balance unchanged, and does not
  initialize a worker, even when the provider delegates to legacy hooks.
- A failed capture-status query still takes the admission path.
- Delegation through all five existing compute hooks and recursive HostArgs
  calls consume one token for the outer operation; errors restore that scope.
- Independent graph replay retains the existing admission behavior.
- Four concurrent callers and a fork inside the provider do not share an
  admission bypass incorrectly.

To test a prebuilt interceptor (including a deployment branch with other fixes):

```sh
HAMI_ARTIFACT_DIR=/path/to/release bash test/host_args.sh
```

This fixture verifies interception and accounting, not NPU execution. On Ascend,
also compare distinct long embedding inputs (full eager prefill) with repeated
inputs (prefix-cache hits that may replay captured graphs). Check real token
consumption as well as successful responses. Repeated identical long prompts
alone do not establish coverage of the eager path.
