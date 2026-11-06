-- SPDX-License-Identifier: AGPL-3.0-only
-- 0013 Maestro version (#73): the version `maestro --version` reported when the run started, and
-- whether the agent's parsers were built for it, from `run.started`. NULL: not probed (an older
-- agent, a command that isn't Maestro, or a probe that failed), so the version is unknown.

ALTER TABLE agent_runs ADD COLUMN maestro_version text;
ALTER TABLE agent_runs ADD COLUMN maestro_profile_supported boolean;
