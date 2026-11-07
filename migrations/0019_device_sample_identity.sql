-- SPDX-License-Identifier: AGPL-3.0-only
-- 0019 device identity and activity on device samples (issue #39, PLAN §6.5). New optional
-- readings from the agent: NULL means the agent did not report it (an older agent, a physical
-- device for the AVD name, or no failed flow for the activity), never zero or empty. The emulator
-- process readings (`emu_cpu_pct`, `emu_rss_bytes`) already exist in 0005.

ALTER TABLE device_samples ADD COLUMN avd_name text;
ALTER TABLE device_samples ADD COLUMN api_level smallint;
ALTER TABLE device_samples ADD COLUMN foreground_activity text;
