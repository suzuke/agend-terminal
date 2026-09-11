use super::{PerTickHandler, TickContext};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) struct ScheduleJobsHandler {
    in_flight: Arc<AtomicBool>,
}
impl ScheduleJobsHandler {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl PerTickHandler for ScheduleJobsHandler {
    fn name(&self) -> &'static str {
        "schedule_jobs"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let guard = super::ClearOnDrop::new(Arc::clone(&self.in_flight));
        let home = ctx.home.to_path_buf();
        let runtime = crate::schedule_jobs::runtime::ManagedRuntime::new(
            ctx.home,
            ctx.registry,
            ctx.configs,
            ctx.externals,
        );
        // fire-and-forget: bounded Job reconciliation offloads spawn/delete from
        // the daemon tick; durable driver lock and ClearOnDrop prevent overlap.
        if let Err(error) = std::thread::Builder::new()
            .name("schedule-jobs".into())
            .spawn(move || {
                let _guard = guard;
                if let Err(error) =
                    crate::schedule_jobs::tick(&home, &runtime, chrono::Utc::now().timestamp())
                {
                    tracing::error!(%error,"job runner failed; durable state retained");
                }
            })
        {
            tracing::error!(%error,"could not start job runner");
        }
    }
}
