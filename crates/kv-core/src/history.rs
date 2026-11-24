//! Undo/redo over [`Project::apply`].
//!
//! Each undo step stores a snapshot of the project before the command. Project
//! state is metadata only (no media, no analysis), so snapshots are small, and
//! undo is exact for every command without per-command inverse logic.

use crate::command::{Command, Event};
use crate::error::CoreError;
use crate::model::Project;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    command: Command,
    /// The state to restore when this step is taken.
    state: Project,
}

/// A project plus its undo and redo stacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    project: Project,
    undo: Vec<Step>,
    redo: Vec<Step>,
}

impl History {
    /// Start a history with no undo or redo steps.
    pub fn new(project: Project) -> Self {
        History {
            project,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// The current project.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Consume the history and return the current project.
    pub fn into_project(self) -> Project {
        self.project
    }

    /// Apply a command. On success it becomes undoable and the redo stack is
    /// cleared; on error nothing changes.
    pub fn apply(&mut self, command: Command) -> Result<Event, CoreError> {
        let mut next = self.project.clone();
        let event = next.apply(command.clone())?;
        let before = std::mem::replace(&mut self.project, next);
        self.undo.push(Step {
            command,
            state: before,
        });
        self.redo.clear();
        Ok(event)
    }

    /// Undo the last applied command. Returns it, or `None` if there is
    /// nothing to undo.
    pub fn undo(&mut self) -> Option<&Command> {
        let step = self.undo.pop()?;
        let after = std::mem::replace(&mut self.project, step.state);
        self.redo.push(Step {
            command: step.command,
            state: after,
        });
        self.redo.last().map(|s| &s.command)
    }

    /// Redo the last undone command. Returns it, or `None` if there is
    /// nothing to redo.
    pub fn redo(&mut self) -> Option<&Command> {
        let step = self.redo.pop()?;
        let before = std::mem::replace(&mut self.project, step.state);
        self.undo.push(Step {
            command: step.command,
            state: before,
        });
        self.undo.last().map(|s| &s.command)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}
