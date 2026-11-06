-- SPDX-License-Identifier: AGPL-3.0-only
-- 0014 host load, cores, swap and pressure (issue #42). New optional host readings from the
-- agent's sampler: NULL means the agent didn't report it (an older agent, or no PSI off Linux),
-- never zero. The one-minute and one-hour aggregates keep the averages of the rolled ones; the
-- core count and swap total are static per machine, so they are raw-only, like host memory total.

ALTER TABLE host_samples ADD COLUMN host_load_1m real;
ALTER TABLE host_samples ADD COLUMN host_cpu_cores integer;
ALTER TABLE host_samples ADD COLUMN host_swap_used_bytes bigint;
ALTER TABLE host_samples ADD COLUMN host_swap_total_bytes bigint;
ALTER TABLE host_samples ADD COLUMN host_psi_cpu_pct real;
ALTER TABLE host_samples ADD COLUMN host_psi_memory_pct real;
ALTER TABLE host_samples ADD COLUMN host_psi_io_pct real;

ALTER TABLE host_rollup_1m ADD COLUMN host_load_1m_avg real;
ALTER TABLE host_rollup_1m ADD COLUMN host_swap_used_avg bigint;
ALTER TABLE host_rollup_1m ADD COLUMN host_psi_cpu_pct_avg real;
ALTER TABLE host_rollup_1m ADD COLUMN host_psi_memory_pct_avg real;
ALTER TABLE host_rollup_1m ADD COLUMN host_psi_io_pct_avg real;

ALTER TABLE host_rollup_1h ADD COLUMN host_load_1m_avg real;
ALTER TABLE host_rollup_1h ADD COLUMN host_swap_used_avg bigint;
ALTER TABLE host_rollup_1h ADD COLUMN host_psi_cpu_pct_avg real;
ALTER TABLE host_rollup_1h ADD COLUMN host_psi_memory_pct_avg real;
ALTER TABLE host_rollup_1h ADD COLUMN host_psi_io_pct_avg real;
