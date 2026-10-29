//! In-memory backend used by tests and future backends as a reference.

use anyhow::{bail, Result};
use std::sync::Mutex;

use super::{BackendInfo, OutputBackend};
use crate::model::layout::LayoutPlan;
use crate::model::output::Output;

#[derive(Debug)]
pub struct FakeBackend {
    outputs: Mutex<Vec<Output>>,
}

impl FakeBackend {
    pub fn new(outputs: Vec<Output>) -> Self {
        Self {
            outputs: Mutex::new(outputs),
        }
    }

    pub fn snapshot(&self) -> Vec<Output> {
        self.outputs.lock().unwrap().clone()
    }
}

impl OutputBackend for FakeBackend {
    fn detect(&self) -> Result<BackendInfo> {
        Ok(BackendInfo {
            supported: true,
            reason: None,
            protocol_version: Some(4),
            warning: None,
        })
    }

    fn list(&self) -> Result<Vec<Output>> {
        Ok(self.snapshot())
    }

    fn dry_run(&self, plan: &LayoutPlan) -> Result<()> {
        let outputs = self.snapshot();
        for (name, po) in &plan.outputs {
            let Some(output) = outputs.iter().find(|o| &o.name == name) else {
                bail!("{name} is not connected");
            };
            if po.enabled && !po.mode.custom && output.find_mode(&po.mode).is_none() {
                bail!("{name}: mode {} is not advertised", po.mode);
            }
            if !(0.5..=3.0).contains(&po.scale) {
                bail!("{name}: scale outside range");
            }
        }
        Ok(())
    }

    fn apply(&self, plan: &LayoutPlan) -> Result<()> {
        self.dry_run(plan)?;
        let mut outputs = self.outputs.lock().unwrap();
        for (name, po) in &plan.outputs {
            if let Some(output) = outputs.iter_mut().find(|o| &o.name == name) {
                output.enabled = po.enabled;
                output.position = po.position;
                output.scale = po.scale;
                output.transform = po.transform;
                output.adaptive_sync = po.adaptive_sync;
                for mode in &mut output.modes {
                    mode.current = mode.same_timing(&po.mode);
                }
                if !output.modes.iter().any(|mode| mode.same_timing(&po.mode)) {
                    let mut mode = po.mode.clone();
                    mode.current = true;
                    output.modes.push(mode);
                }
            }
        }
        Ok(())
    }
}
