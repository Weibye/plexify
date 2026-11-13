use anyhow::Result;
use std::path::PathBuf;

use crate::undo;

/// Command to put a library back the way a fix run found it
pub struct UndoCommand {
    /// The plan file a `validate --fix` run wrote
    plan_file: PathBuf,
    apply: bool,
}

impl UndoCommand {
    pub fn new(plan_file: PathBuf, apply: bool) -> Self {
        Self { plan_file, apply }
    }

    /// Report the reversal, and carry it out if asked to.
    pub async fn execute(&self) -> Result<()> {
        let record = undo::read_record(&self.plan_file)?;
        let plan = undo::plan(&record, &self.plan_file);

        let outcome = if self.apply {
            Some(undo::apply(&plan, &undo::default_record_file())?)
        } else {
            None
        };
        print!("{}", undo::render(&plan, outcome.as_ref()));
        Ok(())
    }
}
