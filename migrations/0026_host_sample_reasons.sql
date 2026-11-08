-- SPDX-License-Identifier: AGPL-3.0-only
-- 0026 host and process sample reasons (issue #39, PLAN §6.5). `unavailable` maps each host or
-- process reading that is NULL to the reason the agent gave for it (`no_psi`, `no_process`,
-- `probe_failed`, …; see `fleet_protocol::frame::SampleUnavailable`). NULL on rows from agents that
-- predate them, and the empty object when the sample had no unknown reading.

ALTER TABLE host_samples ADD COLUMN unavailable jsonb;
