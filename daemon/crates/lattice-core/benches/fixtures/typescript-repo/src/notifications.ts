import type { Incident } from "./models";

export interface NotificationMessage {
  channel: "email" | "slack";
  body: string;
}

export function buildAssignmentNotice(incident: Incident): NotificationMessage {
  return {
    channel: incident.severity === "critical" ? "slack" : "email",
    body: `Incident ${incident.id} assigned`,
  };
}
