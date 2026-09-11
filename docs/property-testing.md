# Property testing

The fork's vsock tests use Proptest 1.11.0 as a workspace-pinned development
dependency. Only its `std` feature is enabled; tests do not install tools,
download fixtures or boot virtual machines. Cargo.lock owns the dependency
versions. The normal Nix state gate includes these tests:

```sh
nix build .#checks.x86_64-linux.unit-state
```

The gate runs 256 cases per property with RNG seed 20260910 and at most 4096
shrink iterations. Inside the pinned development environment, expand or replay
the corpus with:

```sh
PROPTEST_CASES=4096 PROPTEST_RNG_SEED=1234 \
  cargo test --locked --offline -p msb_krun_devices --lib --features blk,net virtio::vsock::
PROPTEST_CASES=4096 PROPTEST_RNG_SEED=1234 \
  cargo test --locked --offline -p msb_krun_vmm --lib \
  --features blk,net,devices/net vmm_config::vsock::tests::
```

Keep generated inputs bounded and valid by construction. Include explicit
reserved values and upper boundaries, not just uniformly random integers.
An independent finite-set model checks allocation; packet tests inspect backend
side effects and retain a valid connection after refused traffic. A parser
round trip alone is not an authorization oracle.

Each case owns its memory, allocator and filesystem resources. Do not mutate
global environment, shared CID counters or live VM state from a property. Keep
concurrent ownership tests separate: a sequential generated sequence is not
proof of race freedom.

Commit new `proptest-regressions` seeds, plus a readable minimized example when
the input establishes a defect. Seeds reproduce a generator's history; concrete
fixtures survive generator changes. The one-byte TSI command regression was
observed failing in the unchanged parser before the payload-bound repair.
Source mutations need isolated build roots and a passing baseline; a compile
failure or timeout is not an assertion detecting a defect.

Native generation/shrinking and packaged VM acceptance remain distinct. Replay
a small, explicitly chosen corpus through disposable VMs after native tests;
do not launch a VM for every shrink step or treat these tests as complete SSH
custody, lifecycle or nested-virtualization acceptance.
