import type { Job } from "../api/types";

const installBlockingStatuses = new Set<Job["status"]>([
  "queued",
  "running",
  "pausing",
  "paused",
]);

export function activeJobCount(jobs: ReadonlyArray<Pick<Job, "status">>): number {
  return jobs.filter((job) => installBlockingStatuses.has(job.status)).length;
}
