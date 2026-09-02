import { describe, expect, it } from "bun:test";
import { collectCounts, type RegistrationSite, type RouteSite } from "../scripts/counts";

/**
 * Two workers registering the same function id is a silent, order-dependent
 * outage: whichever worker connects last owns the id on the bus, and the other
 * worker's capability disappears without an error anywhere. The same is true of
 * two workers claiming one HTTP route.
 *
 * This suite fails on any collision that is not in the allowlist below, and it
 * also fails on any allowlist entry whose collision has been fixed — so the
 * allowlist can only ever shrink.
 */

interface KnownCollision {
  readonly key: string;
  /** ISO date the exception was recorded. */
  readonly since: string;
  /** Where the fix belongs. */
  readonly owner: string;
  readonly reason: string;
}

<<<<<<< HEAD
// TEMPORARY. Recorded 2026-09-02 by the eng-gates remediation, which does not own
// either file. Both fixes are requested in
// /tmp/agentos-remediation/requests/eng-gates.md. Delete the entry, do not edit it.
// Both 2026-09-02 entries were fixed on wp/state-api-sweep before this branch was
// integrated: context-monitor now registers context::trim_micro, and a2a-cards now
// serves GET /api/a2a/agent-card. The allowlist is empty and must stay that way.
=======
// EMPTY, and that is the point. Two collisions were recorded here on 2026-09-02 —
// `context::trim` (context-manager and context-monitor) and
// `GET /.well-known/agent.json` (a2a and a2a-cards). state-api-sweep fixed both
// (`context::trim_micro`, `GET /api/a2a/agent-card`), so the entries were deleted.
// The suite below fails if an entry outlives its collision, so this list can only
// ever shrink. Adding to it needs a date, an owning file and a reason.
>>>>>>> wp/eng-gates
const KNOWN_DUPLICATE_FUNCTION_IDS: KnownCollision[] = [];

const KNOWN_DUPLICATE_HTTP_ROUTES: KnownCollision[] = [];

const ISO_DATE = /^\d{4}-\d{2}-\d{2}$/;
const counts = collectCounts();

function describeSites(sites: readonly (RegistrationSite | RouteSite)[]): string {
  return sites.map((site) => `${site.file}:${site.line}`).join(", ");
}

function expectAllowlistIsSound(allowlist: KnownCollision[]): void {
  const today = new Date().toISOString().slice(0, 10);
  const seen = new Set<string>();
  for (const entry of allowlist) {
    expect(ISO_DATE.test(entry.since), `${entry.key}: since must be an ISO date`).toBe(true);
    expect(entry.since <= today, `${entry.key}: since is in the future`).toBe(true);
    expect(entry.reason.length, `${entry.key}: needs a reason`).toBeGreaterThan(40);
    expect(entry.owner.length, `${entry.key}: needs an owning file`).toBeGreaterThan(0);
    expect(seen.has(entry.key), `${entry.key}: listed twice`).toBe(false);
    seen.add(entry.key);
  }
}

describe("worker registration uniqueness", () => {
  it("registers every function id exactly once outside the dated allowlist", () => {
    const allowed = new Set(KNOWN_DUPLICATE_FUNCTION_IDS.map((entry) => entry.key));
    const unexpected = [...counts.duplicateFunctionIds]
      .filter(([id]) => !allowed.has(id))
      .map(([id, sites]) => `${id} registered by ${describeSites(sites)}`);

    expect(unexpected).toEqual([]);
  });

  it("binds every HTTP route exactly once outside the dated allowlist", () => {
    const allowed = new Set(KNOWN_DUPLICATE_HTTP_ROUTES.map((entry) => entry.key));
    const unexpected = [...counts.duplicateHttpRoutes]
      .filter(([route]) => !allowed.has(route))
      .map(([route, sites]) => `${route} bound by ${describeSites(sites)}`);

    expect(unexpected).toEqual([]);
  });

  it("keeps no allowlist entry alive after its collision is fixed", () => {
    const stale = [
      ...KNOWN_DUPLICATE_FUNCTION_IDS.filter((entry) => !counts.duplicateFunctionIds.has(entry.key)).map(
        (entry) => `function id ${entry.key}`,
      ),
      ...KNOWN_DUPLICATE_HTTP_ROUTES.filter((entry) => !counts.duplicateHttpRoutes.has(entry.key)).map(
        (entry) => `route ${entry.key}`,
      ),
    ];

    expect(
      stale,
      "these collisions are fixed; delete their entries from this file so the allowlist keeps shrinking",
    ).toEqual([]);
  });

  it("never lets the allowlist grow past the 2026-09-02 baseline", () => {
    // Ratcheted from 1 and 1 on 2026-09-02 to 0 and 0 once state-api-sweep landed.
    // Raising either number is a decision, not a fix.
    expect(KNOWN_DUPLICATE_FUNCTION_IDS.length).toBe(0);
    expect(KNOWN_DUPLICATE_HTTP_ROUTES.length).toBe(0);
    expectAllowlistIsSound(KNOWN_DUPLICATE_FUNCTION_IDS);
    expectAllowlistIsSound(KNOWN_DUPLICATE_HTTP_ROUTES);
  });

  it("counts a registration only when it ships, not when a unit test writes one", () => {
    // crates/http-adapter's own tests build `{"api_path": "/api/health", ...}`
    // literals. Those are test fixtures and must not look like route bindings.
    const adapterRoutes = counts.httpRoutes.filter((route) =>
      route.file.startsWith("crates/http-adapter"),
    );
    expect(adapterRoutes).toEqual([]);

    const health = counts.httpRoutes.filter((route) => route.route === "GET /api/health");
    expect(health.map((route) => route.file)).toEqual(["workers/agent-core/src/main.rs"]);
  });
});
