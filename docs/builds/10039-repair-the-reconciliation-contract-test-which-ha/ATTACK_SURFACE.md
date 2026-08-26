# Build 10039 Attack Surface

## Repository object reads

The repaired contract invokes Git only to read fixed paths from the `HEAD`
tree. It does not fetch, consult mutable remote refs, execute file content, or
accept user-controlled paths. Missing blobs, empty required documents, and
missing implementation markers produce explicit assertion failures.

## Regression boundary

The contract protects four durable signals of the reconciled delivery: its
governed invariant, salvage decision, LLM namespace, and fail-closed worker
identity handling. A negative probe deletes a prescribed blob on an isolated
throwaway commit and verifies that the contract fails, preventing a history
subject or other surviving metadata from yielding a false pass.

The source-marker checks intentionally cover the named delivery contract, not
the full semantics of either implementation. Behavioral coverage remains in
the existing Rust, script, and example suites.
