# Durable checkpoint branch

This experimental branch adds `Dialogue::checkpoint`, `restore_checkpoint` and
`continue_with_instruction_limit`. Checkpoint APIs require the `serde` feature.

`DialogueCheckpoint` is an opaque, serializable VM continuation. Capture it at a
public boundary, including a line/command yield, pending option selection,
selection before continuing, fresh `set_node`, or completed dialogue. Restore it
into a freshly configured `Dialogue` with the identical compiled program.

The host must separately persist variables, program identity, RNGs, callback state,
text-provider configuration and undelivered/presented events. Bind fresh storage
and callbacks before restore. Cloning a Dialogue is not an independent transaction:
the default memory variable storage shares its underlying data.

Restore validates the continuation before changing the destination. The checkpoint
format is experimental and makes no compatibility guarantee. It does not embed a
program digest or authenticate saves. The host must bind the saved continuation to
its content and verify envelope integrity before restoring it.

Validation uses bounded abstract control-flow analysis of the trusted program,
with at most 4,096 states and 256 stack/options entries. Unknown dynamic jump labels,
unknown call parameter counts and `Any`-tagged stack values can be rejected. This
is not a verifier for arbitrary or malicious bytecode. Native buffered events are
accepted only as consistent stopped-node startup notifications; ordinary yield and
terminal checkpoints must have drained their native events.

The instruction budget applies to one continue call. Exhaustion returns
`DialogueError::InstructionLimitExceeded`; it does not roll back VM, variables or
callbacks. A host requiring atomicity must restore the previous checkpoint and
its own saved services/state after failure. Fuel does not provide a heap quota.

Focused regressions live in `crates/yarnspinner/tests/checkpoint_tests.rs`:

```sh
cargo test -p yarnspinner --no-default-features --features serde --test checkpoint_tests
```
