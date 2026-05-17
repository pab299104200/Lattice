#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub action: String,
    pub target: String,
}

pub trait AuditSink {
    fn record(&mut self, event: AuditEvent);
}

#[derive(Default)]
pub struct VecAuditSink {
    events: Vec<AuditEvent>,
}

impl VecAuditSink {
    pub fn events(&self) -> &[AuditEvent] {
        &self.events
    }
}

impl AuditSink for VecAuditSink {
    fn record(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}

pub fn audit_schedule<S: AuditSink>(sink: &mut S, job_name: &str) {
    sink.record(AuditEvent {
        action: "schedule".to_string(),
        target: job_name.to_string(),
    });
}
