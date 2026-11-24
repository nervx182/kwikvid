//! Undo/redo over [`Project::apply`].
//!
//! Each undo step stores a snapshot of the project before the command. Project
//! state is metadata only (no media, no analysis), so snapshots are small, and
//! undo is exact for every command without per-command inverse logic.
//!
//! [`History::apply_group`] records several commands as one undo step, so one
//! user action that expands into several commands (a pin plus the regenerated
//! edit, for example) is undone and redone as a whole.

use crate::command::{Command, Event};
use crate::error::CoreError;
use crate::model::Project;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    /// One or more commands; the first is the one the user asked for.
    commands: Vec<Command>,
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
            commands: vec![command],
            state: before,
        });
        self.redo.clear();
        Ok(event)
    }

    /// Apply several commands in order as **one** undo step. On success the
    /// redo stack is cleared and the events are returned in order; if any
    /// command fails, nothing changes. An empty group changes nothing and
    /// records no step. [`History::undo`] and [`History::redo`] report the
    /// group by its first command.
    pub fn apply_group(&mut self, commands: Vec<Command>) -> Result<Vec<Event>, CoreError> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let mut next = self.project.clone();
        let events = commands
            .iter()
            .map(|c| next.apply(c.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let before = std::mem::replace(&mut self.project, next);
        self.undo.push(Step {
            commands,
            state: before,
        });
        self.redo.clear();
        Ok(events)
    }

    /// Undo the last applied command (or group). Returns it (a group's first
    /// command), or `None` if there is nothing to undo.
    pub fn undo(&mut self) -> Option<&Command> {
        let step = self.undo.pop()?;
        let after = std::mem::replace(&mut self.project, step.state);
        self.redo.push(Step {
            commands: step.commands,
            state: after,
        });
        self.redo.last().map(|s| &s.commands[0])
    }

    /// Redo the last undone command (or group). Returns it (a group's first
    /// command), or `None` if there is nothing to redo.
    pub fn redo(&mut self) -> Option<&Command> {
        let step = self.redo.pop()?;
        let before = std::mem::replace(&mut self.project, step.state);
        self.undo.push(Step {
            commands: step.commands,
            state: before,
        });
        self.undo.last().map(|s| &s.commands[0])
    }

    /// Rewrite every media and music path held by the history: the current
    /// project, every undo and redo state, and the paths inside stored
    /// `AddMedia` / `SetMusic` commands. Used when the folder the paths are
    /// relative to changes (Save As); this is not an edit, so it is not an
    /// undo step and the stacks keep their length.
    ///
    /// On error the history may be partly rewritten; call it on a clone to
    /// keep the original.
    pub fn try_map_paths<E>(
        &mut self,
        mut f: impl FnMut(&str) -> Result<String, E>,
    ) -> Result<(), E> {
        self.project.try_map_paths(&mut f)?;
        for step in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            step.state.try_map_paths(&mut f)?;
            for command in &mut step.commands {
                match command {
                    Command::AddMedia { item } => item.path = f(&item.path)?,
                    Command::SetMusic {
                        music: Some(music), ..
                    } => music.path = f(&music.path)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Project, ProjectId};

    const S: crate::SectionId = Project::DEFAULT_SECTION;

    fn seed(seed: u64) -> Command {
        Command::SetSeed { section: S, seed }
    }

    #[test]
    fn group_is_one_undo_step() {
        let mut h = History::new(Project::new(ProjectId(1)));
        h.apply(seed(1)).unwrap();
        let events = h.apply_group(vec![seed(2), seed(3)]).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(h.project().sections[0].seed, 3);
        assert_eq!(h.undo(), Some(&seed(2)));
        assert_eq!(h.project().sections[0].seed, 1);
        assert_eq!(h.redo(), Some(&seed(2)));
        assert_eq!(h.project().sections[0].seed, 3);
    }

    #[test]
    fn failing_group_changes_nothing() {
        let mut h = History::new(Project::new(ProjectId(1)));
        let bad = Command::SetSeed {
            section: crate::SectionId(99),
            seed: 1,
        };
        assert!(h.apply_group(vec![seed(5), bad]).is_err());
        assert_eq!(h.project().sections[0].seed, 0);
        assert!(!h.can_undo());
        assert_eq!(h.apply_group(Vec::new()), Ok(Vec::new()));
        assert!(!h.can_undo());
    }
}
