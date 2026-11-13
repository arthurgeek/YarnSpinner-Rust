//! Prototype continuation API. Runtime services and program identity stay host-owned.

use super::{ExecutionState, State, VirtualMachine};
use crate::prelude::*;
use core::fmt;
use yarnspinner_core::types::{Type, TypedValue};

/// An opaque continuation at an externally observable VM boundary.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogueCheckpoint {
    version: u32,
    node: Option<String>,
    state: State,
    execution: ExecutionState,
    events: Vec<DialogueEvent>,
    language: Option<Language>,
    line_hints: bool,
}

impl DialogueCheckpoint {
    /// Frozen pending options, empty outside an option boundary.
    pub fn pending_options(&self) -> &[DialogueOption] {
        &self.state.current_options
    }
    /// The saved narrative language; providers are rebound by the host.
    pub fn language_code(&self) -> Option<&Language> {
        self.language.as_ref()
    }
}

/// Invalid continuation or an unsupported capture boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    /// There is no loaded program.
    NoProgram,
    /// The checkpoint version is unsupported.
    Version(u32),
    /// Running instructions cannot be captured/restored as a yielded boundary.
    Running,
    /// The named node is absent from the loaded program.
    Node(String),
    /// Program counter or execution state is inconsistent.
    Location,
    /// Pending options or their destination labels are inconsistent.
    Options,
    /// The value stack is inconsistent with a yielded boundary.
    Stack,
    /// Trusted-program control-flow analysis exceeded its prototype work bound.
    AnalysisLimit,
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProgram => f.write_str("checkpoint requires a loaded program"),
            Self::Version(version) => write!(f, "unsupported checkpoint version {version}"),
            Self::Running => f.write_str("checkpoint requires a yielded VM boundary"),
            Self::Node(node) => write!(f, "checkpoint node is absent: {node}"),
            Self::Location => f.write_str("checkpoint instruction location is invalid"),
            Self::Options => f.write_str("checkpoint options are inconsistent"),
            Self::Stack => f.write_str("checkpoint stack is inconsistent"),
            Self::AnalysisLimit => f.write_str("checkpoint validation analysis limit exceeded"),
        }
    }
}

impl core::error::Error for CheckpointError {}

impl VirtualMachine {
    pub(crate) fn checkpoint(&self) -> core::result::Result<DialogueCheckpoint, CheckpointError> {
        let mut checkpoint = DialogueCheckpoint {
            version: 1,
            node: self.current_node_name.clone(),
            state: self.state.clone(),
            execution: self.execution_state,
            events: self.batched_events.clone(),
            language: self.language_code.clone(),
            line_hints: self.line_hints_enabled,
        };
        // Upstream Stop and empty ShowOptions increment PC after clearing the node.
        // That counter has no meaning once stopped; expose one canonical terminal state.
        if checkpoint.node.is_none()
            && checkpoint.execution == ExecutionState::Stopped
            && checkpoint.state.stack.is_empty()
            && checkpoint.state.current_options.is_empty()
        {
            checkpoint.state = State::default();
        }
        self.validate_checkpoint(&checkpoint)?;
        Ok(checkpoint)
    }

    fn validate_checkpoint(
        &self,
        checkpoint: &DialogueCheckpoint,
    ) -> core::result::Result<(), CheckpointError> {
        let program = self.program.as_ref().ok_or(CheckpointError::NoProgram)?;
        if checkpoint.version != 1 {
            return Err(CheckpointError::Version(checkpoint.version));
        }
        if checkpoint.execution == ExecutionState::Running {
            return Err(CheckpointError::Running);
        }
        // continue_ drains native events at each public yield. Only set_node can
        // leave startup notifications queued outside instruction execution.
        if !checkpoint.events.is_empty() {
            if checkpoint.execution != ExecutionState::Stopped || checkpoint.node.is_none() {
                return Err(CheckpointError::Location);
            }
            let mut started = None;
            let mut hinted = false;
            for event in &checkpoint.events {
                match event {
                    DialogueEvent::NodeStart(name) if program.nodes.contains_key(name) => {
                        started = Some(name);
                        hinted = false;
                    }
                    DialogueEvent::LineHints(ids) if !hinted => {
                        let node = started
                            .and_then(|name| program.nodes.get(name))
                            .ok_or(CheckpointError::Location)?;
                        let expected: Vec<LineId> = node
                            .instructions
                            .iter()
                            .filter(|instruction| {
                                matches!(
                                    OpCode::try_from(instruction.opcode),
                                    Ok(OpCode::RunLine | OpCode::AddOption)
                                )
                            })
                            .filter_map(|instruction| {
                                instruction
                                    .operands
                                    .first()
                                    .and_then(|operand| String::try_from(operand.clone()).ok())
                                    .map(LineId)
                            })
                            .collect();
                        if ids != &expected {
                            return Err(CheckpointError::Location);
                        }
                        hinted = true;
                    }
                    _ => return Err(CheckpointError::Location),
                }
            }
            if started != checkpoint.node.as_ref() {
                return Err(CheckpointError::Location);
            }
        }
        let Some(name) = &checkpoint.node else {
            return if checkpoint.execution == ExecutionState::Stopped
                && checkpoint.state == State::default()
            {
                Ok(())
            } else {
                Err(CheckpointError::Location)
            };
        };
        let node = program
            .nodes
            .get(name)
            .ok_or_else(|| CheckpointError::Node(name.clone()))?;
        if checkpoint.state.program_counter >= node.instructions.len() {
            return Err(CheckpointError::Location);
        }
        for value in &checkpoint.state.stack {
            if value.r#type != value.raw_value.r#type()
                || matches!(&value.raw_value, YarnValue::Number(number) if !number.is_finite())
            {
                return Err(CheckpointError::Stack);
            }
        }
        if checkpoint.execution == ExecutionState::Stopped {
            return if checkpoint.state == State::default() {
                Ok(())
            } else {
                Err(CheckpointError::Location)
            };
        }
        self.validate_yield_signature(node, program, checkpoint)?;
        Ok(())
    }

    // This checks the trusted compiler's node, not arbitrary bytecode safety. No
    // callback is invoked and no host variable is read or changed. Unknown runtime
    // values retain their types; literal labels/counts retain their exact values.
    fn validate_yield_signature(
        &self,
        node: &Node,
        program: &Program,
        checkpoint: &DialogueCheckpoint,
    ) -> core::result::Result<(), CheckpointError> {
        const MAX_STATES: usize = 4096;
        const MAX_STACK: usize = 256;
        let mut work = vec![AbstractState::default()];
        let mut seen = Vec::new();
        while let Some(mut state) = work.pop() {
            if seen.contains(&state) {
                continue;
            }
            if seen.len() >= MAX_STATES
                || state.stack.len() > MAX_STACK
                || state.options.len() > MAX_STACK
            {
                return Err(CheckpointError::AnalysisLimit);
            }
            seen.push(state.clone());
            let instruction = node
                .instructions
                .get(state.pc)
                .ok_or(CheckpointError::Location)?;
            let opcode =
                OpCode::try_from(instruction.opcode).map_err(|_| CheckpointError::Location)?;
            state.pc += 1;
            match opcode {
                OpCode::PushString => state
                    .stack
                    .push(Symbol::Known(YarnValue::String(operand(instruction, 0)?))),
                OpCode::PushFloat => state
                    .stack
                    .push(Symbol::Known(YarnValue::Number(operand(instruction, 0)?))),
                OpCode::PushBool => state
                    .stack
                    .push(Symbol::Known(YarnValue::Boolean(operand(instruction, 0)?))),
                OpCode::PushVariable => {
                    let name: String = operand(instruction, 0)?;
                    let value: YarnValue = program
                        .initial_values
                        .get(&name)
                        .ok_or(CheckpointError::Stack)?
                        .clone()
                        .into();
                    state.stack.push(Symbol::Kind(value.r#type()));
                }
                OpCode::StoreVariable => {
                    state.stack.last().ok_or(CheckpointError::Stack)?;
                }
                OpCode::Pop => {
                    state.stack.pop().ok_or(CheckpointError::Stack)?;
                }
                OpCode::JumpTo => {
                    state.pc = label_pc(node, &operand::<String>(instruction, 0)?)?;
                }
                OpCode::Jump => {
                    let Some(Symbol::Known(YarnValue::String(label))) = state.stack.last() else {
                        return Err(CheckpointError::Stack);
                    };
                    state.pc = label_pc(node, label)?;
                }
                OpCode::JumpIfFalse => {
                    require_type(
                        state.stack.last().ok_or(CheckpointError::Stack)?,
                        &Type::Boolean,
                    )?;
                    let mut branch = state.clone();
                    branch.pc = label_pc(node, &operand::<String>(instruction, 0)?)?;
                    work.push(branch);
                }
                OpCode::CallFunc => {
                    let count = state.stack.pop().ok_or(CheckpointError::Stack)?.count()?;
                    let name: String = operand(instruction, 0)?;
                    let function = self.library.get(&name).ok_or(CheckpointError::Stack)?;
                    let types = function.parameter_types();
                    if types.len() != count {
                        return Err(CheckpointError::Stack);
                    }
                    let start = state
                        .stack
                        .len()
                        .checked_sub(count)
                        .ok_or(CheckpointError::Stack)?;
                    for (argument, expected) in state.stack[start..].iter().zip(types) {
                        let expected =
                            Type::try_from(expected).map_err(|_| CheckpointError::Stack)?;
                        require_type(argument, &expected)?;
                    }
                    state.stack.truncate(start);
                    let result = Type::try_from(function.return_type())
                        .map_err(|_| CheckpointError::Stack)?;
                    state.stack.push(Symbol::Kind(result));
                }
                OpCode::RunLine | OpCode::RunCommand | OpCode::AddOption => {
                    let count: usize =
                        operand(instruction, if opcode == OpCode::AddOption { 2 } else { 1 })?;
                    let start = state
                        .stack
                        .len()
                        .checked_sub(count)
                        .ok_or(CheckpointError::Stack)?;
                    state.stack.truncate(start);
                    if opcode == OpCode::AddOption {
                        if operand::<bool>(instruction, 3)? {
                            require_type(
                                &state.stack.pop().ok_or(CheckpointError::Stack)?,
                                &Type::Boolean,
                            )?;
                        }
                        let line = operand::<String>(instruction, 0)?;
                        let label = operand::<String>(instruction, 1)?;
                        label_pc(node, &label)?;
                        state.options.push((line, label));
                    } else if state.matches(checkpoint, ExecutionState::WaitingForContinue) {
                        return Ok(());
                    }
                }
                OpCode::ShowOptions => {
                    if state.options.is_empty() {
                        continue;
                    }
                    if state.matches(checkpoint, ExecutionState::WaitingOnOptionSelection) {
                        return Ok(());
                    }
                    for (_, label) in core::mem::take(&mut state.options) {
                        let mut selected = state.clone();
                        selected.stack.push(Symbol::Known(YarnValue::String(label)));
                        if selected.matches(checkpoint, ExecutionState::WaitingForContinue) {
                            return Ok(());
                        }
                        work.push(selected);
                    }
                    continue;
                }
                OpCode::RunNode => {
                    require_type(
                        &state.stack.pop().ok_or(CheckpointError::Stack)?,
                        &Type::String,
                    )?;
                    // set_node resets the stack; the destination is independently
                    // checked from its own entry when capturing that node.
                    continue;
                }
                OpCode::Stop => continue,
                OpCode::PushNull => return Err(CheckpointError::Stack),
            }
            if state.pc < node.instructions.len() {
                work.push(state);
            }
        }
        Err(CheckpointError::Stack)
    }

    pub(crate) fn restore_checkpoint(
        &mut self,
        checkpoint: DialogueCheckpoint,
    ) -> core::result::Result<(), CheckpointError> {
        self.validate_checkpoint(&checkpoint)?;
        let node = match &checkpoint.node {
            Some(name) => Some(
                self.program
                    .as_ref()
                    .and_then(|program| program.nodes.get(name))
                    .ok_or_else(|| CheckpointError::Node(name.clone()))?
                    .clone(),
            ),
            None => None,
        };
        self.set_language_code(checkpoint.language);
        self.current_node_name = checkpoint.node;
        self.current_node = node;
        self.state = checkpoint.state;
        self.execution_state = checkpoint.execution;
        self.batched_events = checkpoint.events;
        self.line_hints_enabled = checkpoint.line_hints;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Symbol {
    Known(YarnValue),
    Kind(Type),
}

impl Symbol {
    fn kind(&self) -> Type {
        match self {
            Self::Known(value) => value.r#type(),
            Self::Kind(kind) => kind.clone(),
        }
    }

    fn count(&self) -> core::result::Result<usize, CheckpointError> {
        match self {
            Self::Known(value) => {
                usize::try_from(value.clone()).map_err(|_| CheckpointError::Stack)
            }
            Self::Kind(_) => Err(CheckpointError::Stack),
        }
    }

    fn matches(&self, actual: &InternalValue) -> bool {
        match self {
            Self::Known(value) => value == &actual.raw_value,
            Self::Kind(kind) => kind == &Type::Any || kind == &actual.raw_value.r#type(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct AbstractState {
    pc: usize,
    stack: Vec<Symbol>,
    options: Vec<(String, String)>,
}

impl AbstractState {
    fn matches(&self, checkpoint: &DialogueCheckpoint, execution: ExecutionState) -> bool {
        checkpoint.execution == execution
            && self.pc == checkpoint.state.program_counter
            && self.stack.len() == checkpoint.state.stack.len()
            && self
                .stack
                .iter()
                .zip(&checkpoint.state.stack)
                .all(|(expected, actual)| expected.matches(actual))
            && self.options.len() == checkpoint.state.current_options.len()
            && self
                .options
                .iter()
                .zip(&checkpoint.state.current_options)
                .enumerate()
                .all(|(index, ((line, label), actual))| {
                    actual.id.0 == index
                        && &actual.line.id.0 == line
                        && &actual.destination_node == label
                })
    }
}

fn require_type(symbol: &Symbol, expected: &Type) -> core::result::Result<(), CheckpointError> {
    if expected == &Type::Any || &symbol.kind() == expected {
        Ok(())
    } else {
        Err(CheckpointError::Stack)
    }
}

fn operand<T: TryFrom<Operand>>(
    instruction: &Instruction,
    index: usize,
) -> core::result::Result<T, CheckpointError> {
    instruction
        .operands
        .get(index)
        .cloned()
        .ok_or(CheckpointError::Location)?
        .try_into()
        .map_err(|_| CheckpointError::Location)
}

fn label_pc(node: &Node, name: &str) -> core::result::Result<usize, CheckpointError> {
    let pc = node
        .labels
        .get(name)
        .and_then(|pc| usize::try_from(*pc).ok())
        .ok_or(CheckpointError::Options)?;
    if pc < node.instructions.len() {
        Ok(pc)
    } else {
        Err(CheckpointError::Options)
    }
}
