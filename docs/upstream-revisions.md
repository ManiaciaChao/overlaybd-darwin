# Dependency provenance

The `vendor/agentenv` submodule pins
`449180d7389d8d0650c66c2ed2ef55db01d26708` from the `feat/packed-upper`
branch of [ManiaciaChao/AgentENV](https://github.com/ManiaciaChao/AgentENV).
Its upstream base is `bc51e699421fdf67865665f36269b66a1ffac479` from
[kvcache-ai/AgentENV](https://github.com/kvcache-ai/AgentENV).

The pin includes packed storage, policy, lifecycle fixes, counter APIs and the
historical writable ZFile/VDO paths. Packed does not use a VDO worker. Existing
license and copyright files are retained. The Darwin repository and native
storage fork remain separate repositories.

The native branch was pushed to the owner's fork as `feat/packed-upper` on
2026-11-02 using the `ManiaciaChao` GitHub account. Its remote tip matches the
pinned commit. Publish native changes first and verify the exact pin is fetchable
before publishing future Darwin parent updates.
