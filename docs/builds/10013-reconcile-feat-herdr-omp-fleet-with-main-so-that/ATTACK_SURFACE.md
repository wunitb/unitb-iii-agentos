# Build 10013 Attack Surface

## Model-produced tool calls

Provider completion JSON is untrusted. Before a call reaches the capability
check or iii function trigger, the router and agent-core require typed,
non-empty call and function identifiers. Unknown provider aliases, missing
required fields, wrong JSON types, and empty normalized identifiers are
discarded. Arguments remain JSON data and are never interpolated as code.

## Gemini identifier synthesis

Gemini call IDs are optional, so absent or empty string IDs receive a stable
request-local identifier derived from candidate and part indexes. The function
name must still resolve through the request's alias map, and a non-string ID is
rejected. Synthesis therefore supplies only correlation metadata; it cannot
select or authorize a function.

## Continuation loop

An array containing only malformed calls now terminates processing. Agent-core
does not append that raw array to assistant history and does not spend another
model request on a structurally invalid continuation. Mixed arrays execute
only successfully normalized calls; provider adapters also filter normalized
history before producing native continuation payloads.

## Preserved operational boundaries

Worker discovery remains fail-closed when engine identity state is unknown.
Portable installer upgrades continue to preserve operator-owned state. Secure
local routing remains restricted to loopback addresses, and provider secrets
retain their provider-specific transport mechanisms.
