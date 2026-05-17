export interface AssignmentGroup {
  id: string;
  name: string;
  active: boolean;
}

export interface Incident {
  id: string;
  title: string;
  severity: "low" | "medium" | "high" | "critical";
  assignmentGroupId?: string;
}

export class IncidentQueue {
  private readonly incidents: Incident[];

  constructor(incidents: Incident[]) {
    this.incidents = incidents;
  }

  assignIncident(incidentId: string, group: AssignmentGroup): Incident {
    const incident = this.incidents.find((candidate) => candidate.id === incidentId);
    if (!incident) {
      throw new Error("Incident not found");
    }
    return assignIncident(incident, group);
  }
}

export function assignIncident(incident: Incident, group: AssignmentGroup): Incident {
  if (!group.active) {
    throw new Error("Assignment group is inactive");
  }
  return { ...incident, assignmentGroupId: group.id };
}
