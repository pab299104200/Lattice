import { describe, expect, it } from "vitest";
import { IncidentQueue, assignIncident } from "../src/models";

describe("incident assignment", () => {
  it("assigns an active group", () => {
    const incident = { id: "inc-1", title: "Disk full", severity: "high" as const };
    const group = { id: "grp-1", name: "Operations", active: true };

    expect(assignIncident(incident, group).assignmentGroupId).toBe("grp-1");
  });

  it("throws when the incident cannot be found", () => {
    const queue = new IncidentQueue([]);
    const group = { id: "grp-1", name: "Operations", active: true };

    expect(() => queue.assignIncident("missing", group)).toThrow("Incident not found");
  });
});
