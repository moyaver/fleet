-- SPDX-License-Identifier: AGPL-3.0-only
-- 0024 device sample shard and reasons (issue #39, PLAN §6.5). `shard` is the shard index the
-- device was assigned (the `shards.shard_index` of its session). `unavailable` maps each reading
-- that is NULL to the reason the agent gave for it (`not_an_emulator`, `probe_failed`, …; see
-- `fleet_protocol::frame::Unavailable`). NULL on rows from agents that predate them, and the
-- empty object when the sample had no unknown reading.

ALTER TABLE device_samples ADD COLUMN shard integer CHECK (shard >= 1);
ALTER TABLE device_samples ADD COLUMN unavailable jsonb;
