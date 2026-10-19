# Implementing and checking a provider

Implement the shared `Provider` trait: model identity, per-model capabilities,
completion, and streaming. Declare cache capabilities only for cache mechanisms
that the adapter actually maps. Call `ChatRequest::validate_cache` before sending;
request fitting and fallible cache validation are deliberately separate steps.
Unknown providers remain usable, while unsupported explicit cache controls fail.

Use `test_support::contract` with deterministic responses. Its bounded helpers
check completion content, real stream termination, typed acquisition/stream errors,
tool IDs, cumulative usage progress, reasoning as a subset of output, and idempotent request
fitting. Failure messages contain no response content or provider diagnostics.
The tests `provider_contract`, `provider_contract_claude`, and the application's
`MockProvider` tests show vendor-neutral, fake HTTP, and application integrations.
These fixtures do not establish live service compatibility or pricing accuracy.

A provider adapter still needs its own protocol tests: fragmented UTF-8 and JSON,
transport truncation, malformed/oversized events, tool argument assembly, refusal,
and cache wire mapping cannot be proved by the common trait alone. Tool results
must retain IDs and content; unsupported modalities must be rejected or explicitly
reported rather than silently presented as supported.

`Agent::provider` selects the initial provider; `register_provider` registers
additional model handles. `AgentCx::set_model` changes the routed model between
steps. Capability lookup must use the requested model rather than a default
handle. Separate credentials and compatible endpoints belong to each client.
Application catalogs, plan gates, pricing, and configuration require registration
too: implementing this trait does not automatically enable an application model.

The cache strategy, opaque scope key, usage buckets, and validation seam are shared.
Usage progress retains billed work without signaling completion or authorizing tools.
Physical prompt caches remain provider-managed; a hit at one vendor is not a hit at
another. Explicit cache TTLs and point limits are declared per provider/model.
