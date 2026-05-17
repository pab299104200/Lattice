import type { Incident } from "./models";
import { buildAssignmentNotice } from "./notifications";

interface IncidentSummaryProps {
  incident: Incident;
}

export function IncidentSummary({ incident }: IncidentSummaryProps) {
  const notice = buildAssignmentNotice(incident);
  return (
    <section aria-label="Incident summary">
      <h2>{incident.title}</h2>
      <p>{notice.body}</p>
    </section>
  );
}
