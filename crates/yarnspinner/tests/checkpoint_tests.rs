#![cfg(feature = "serde")]
#![expect(
    clippy::unwrap_used,
    reason = "prototype regressions assert successful fixture setup"
)]

use yarnspinner::{
    compiler::{Compiler, File},
    runtime::{
        Dialogue, DialogueCheckpoint, DialogueEvent, MemoryVariableStorage, StringTableTextProvider,
    },
};

fn fixture() -> Dialogue {
    let compilation = Compiler::new().add_file(File { file_name: "checkpoint.yarn".into(), source: "title: Start\n---\n<<declare $value = 7>>\n-> Continue {$value} #line:continue\n    <<set $value = $value + 1>>\n    Done {$value} #line:done\n===".into() }).compile().unwrap();
    let mut provider = StringTableTextProvider::new();
    provider.extend_base_language(
        compilation
            .string_table
            .into_iter()
            .map(|(id, info)| (id, info.text)),
    );
    let mut dialogue = Dialogue::new(Box::new(MemoryVariableStorage::new()), Box::new(provider));
    dialogue.add_program(compilation.program.unwrap());
    dialogue.set_node("Start").unwrap();
    dialogue
}

#[test]
fn opaque_vm_checkpoint_restores_a_waiting_choice_without_replaying_the_node() {
    let mut original = fixture();
    let events = original.continue_().unwrap();
    let options = events
        .iter()
        .find_map(|event| {
            if let DialogueEvent::Options(options) = event {
                Some(options.clone())
            } else {
                None
            }
        })
        .unwrap();
    let encoded = serde_json::to_vec(&original.checkpoint().unwrap()).unwrap();
    let checkpoint: DialogueCheckpoint = serde_json::from_slice(&encoded).unwrap();
    let variables = original.variable_storage().variables();
    let mut restored = fixture();
    restored.variable_storage_mut().extend(variables).unwrap();
    restored.restore_checkpoint(checkpoint).unwrap();
    assert!(restored.is_waiting_for_option_selection());
    original.set_selected_option(options[0].id).unwrap();
    restored.set_selected_option(options[0].id).unwrap();
    assert_eq!(original.continue_().unwrap(), restored.continue_().unwrap());
    assert_eq!(
        original.variable_storage().variables(),
        restored.variable_storage().variables()
    );
}

fn source_fixture(source: &str) -> Dialogue {
    let compilation = Compiler::new()
        .add_file(File {
            file_name: "boundary.yarn".into(),
            source: source.into(),
        })
        .compile()
        .unwrap();
    let mut provider = StringTableTextProvider::new();
    provider.extend_base_language(
        compilation
            .string_table
            .into_iter()
            .map(|(id, info)| (id, info.text)),
    );
    let mut dialogue = Dialogue::new(Box::new(MemoryVariableStorage::new()), Box::new(provider));
    dialogue.add_program(compilation.program.unwrap());
    dialogue.set_node("Start").unwrap();
    dialogue
}

#[test]
fn completed_stop_has_a_restorable_canonical_checkpoint() {
    let mut dialogue = source_fixture("title: Start\n---\n<<stop>>\n===");
    dialogue.continue_().unwrap();
    let checkpoint = dialogue.checkpoint().unwrap();
    let mut restored = fixture();
    restored.restore_checkpoint(checkpoint.clone()).unwrap();
    assert!(!restored.is_active());
    assert_eq!(restored.checkpoint().unwrap(), checkpoint);
}

fn rejects_without_mutating(value: serde_json::Value) {
    rejects_into_without_mutating(fixture(), value);
}

fn rejects_into_without_mutating(mut target: Dialogue, value: serde_json::Value) {
    let before = target.checkpoint().unwrap();
    let altered: DialogueCheckpoint = serde_json::from_value(value).unwrap();
    assert!(target.restore_checkpoint(altered).is_err());
    assert_eq!(target.checkpoint().unwrap(), before);
}

#[test]
fn rejects_in_range_pc_that_is_not_a_yield_boundary() {
    let mut dialogue = fixture();
    dialogue.continue_().unwrap();
    let mut value = serde_json::to_value(dialogue.checkpoint().unwrap()).unwrap();
    value["state"]["program_counter"] = 1.into();
    value["state"]["current_options"] = serde_json::json!([]);
    value["execution"] = "WaitingForContinue".into();
    rejects_without_mutating(value);
}

#[test]
fn rejects_missing_conditional_stack_at_a_line_boundary() {
    let source = "title: Start\n---\n<<declare $value = 7>>\n<<if $value > 0>>\nInside #line:inside\n<<endif>>\n===";
    let mut dialogue = source_fixture(source);
    dialogue.continue_().unwrap();
    let mut value = serde_json::to_value(dialogue.checkpoint().unwrap()).unwrap();
    assert!(!value["state"]["stack"].as_array().unwrap().is_empty());
    value["state"]["stack"] = serde_json::json!([]);
    rejects_into_without_mutating(source_fixture(source), value);
}

#[test]
fn restore_retains_initial_batched_events_and_line_hints() {
    let mut original = fixture();
    original.set_line_hints_enabled(true);
    original.set_node("Start").unwrap();
    let checkpoint = original.checkpoint().unwrap();
    let mut restored = fixture();
    restored.restore_checkpoint(checkpoint).unwrap();
    assert!(restored.line_hints_enabled());
    let events = original.continue_().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DialogueEvent::NodeStart(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DialogueEvent::LineHints(_)))
    );
    assert_eq!(restored.continue_().unwrap(), events);
}

#[test]
fn restores_after_selection_before_execution() {
    let mut original = fixture();
    original.continue_().unwrap();
    let option = original.checkpoint().unwrap().pending_options()[0].id;
    original.set_selected_option(option).unwrap();
    let checkpoint = original.checkpoint().unwrap();
    let mut restored = fixture();
    restored.restore_checkpoint(checkpoint).unwrap();
    assert_eq!(restored.continue_().unwrap(), original.continue_().unwrap());
}

#[test]
fn command_and_conditional_line_yields_restore_without_replay() {
    let source = "title: Start\n---\n<<declare $value = 7>>\n<<if $value > 0>>\n<<record marker>>\nInside #line:inside\n<<endif>>\nFinished #line:finished\n===";
    let mut original = source_fixture(source);
    assert!(
        original
            .continue_()
            .unwrap()
            .iter()
            .any(|event| matches!(event, DialogueEvent::Command(_)))
    );
    for _ in 0..2 {
        let mut restored = source_fixture(source);
        restored
            .restore_checkpoint(original.checkpoint().unwrap())
            .unwrap();
        assert_eq!(restored.continue_().unwrap(), original.continue_().unwrap());
    }
}

#[test]
fn invalid_version_node_pc_options_and_stack_leave_target_unchanged() {
    let mut dialogue = fixture();
    dialogue.continue_().unwrap();
    let baseline = serde_json::to_value(dialogue.checkpoint().unwrap()).unwrap();
    let mut value = baseline.clone();
    value["version"] = 999.into();
    rejects_without_mutating(value);
    let mut value = baseline.clone();
    value["node"] = "missing".into();
    rejects_without_mutating(value);
    let mut value = baseline.clone();
    value["state"]["program_counter"] = usize::MAX.into();
    rejects_without_mutating(value);
    let mut value = baseline.clone();
    value["state"]["current_options"][0]["destination_node"] = "missing".into();
    rejects_without_mutating(value);
    let mut value = baseline;
    value["state"]["stack"] = serde_json::json!([{"type":"Boolean", "raw_value":{"Boolean":true}}]);
    rejects_without_mutating(value);
}

#[test]
fn a_real_label_from_another_option_is_not_valid_option_provenance() {
    let source = "title: Start\n---\n-> First #line:first\n    First branch #line:branch1\n-> Second #line:second\n    Second branch #line:branch2\n===";
    let mut dialogue = source_fixture(source);
    dialogue.continue_().unwrap();
    let mut value = serde_json::to_value(dialogue.checkpoint().unwrap()).unwrap();
    value["state"]["current_options"][0]["destination_node"] =
        value["state"]["current_options"][1]["destination_node"].clone();
    rejects_into_without_mutating(source_fixture(source), value);
}

#[test]
fn rejects_wrong_stack_kind_at_an_actual_yield() {
    let source = "title: Start\n---\n<<declare $value = 7>>\n<<if $value > 0>>\nInside #line:inside\n<<endif>>\n===";
    let mut dialogue = source_fixture(source);
    dialogue.continue_().unwrap();
    let mut value = serde_json::to_value(dialogue.checkpoint().unwrap()).unwrap();
    value["state"]["stack"][0] = serde_json::json!({"type":"String", "raw_value":{"String":"bad"}});
    rejects_into_without_mutating(source_fixture(source), value);
}

#[test]
fn drained_yield_boundaries_reject_buffered_native_events() {
    let mut dialogue = fixture();
    dialogue.continue_().unwrap();
    let mut value = serde_json::to_value(dialogue.checkpoint().unwrap()).unwrap();
    value["events"] = serde_json::json!(["DialogueComplete"]);
    rejects_without_mutating(value);
}

#[test]
fn instruction_limit_returns_a_typed_error_and_allows_explicit_rollback() {
    let mut dialogue = source_fixture("title: Start\n---\n<<jump Start>>\n===");
    let before = dialogue.checkpoint().unwrap();
    assert!(matches!(
        dialogue.continue_with_instruction_limit(32),
        Err(yarnspinner::runtime::DialogueError::InstructionLimitExceeded { limit: 32 })
    ));
    dialogue.restore_checkpoint(before.clone()).unwrap();
    assert_eq!(dialogue.checkpoint().unwrap(), before);
}
