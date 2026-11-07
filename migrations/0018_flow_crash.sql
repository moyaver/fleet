-- SPDX-License-Identifier: AGPL-3.0-only
-- The app crash Maestro recorded during a flow (`app.crashed`, issue #152). Evidence only: it never
-- changes the flow's state, outcome or failure (I3). Both columns are null until a crash arrives,
-- and a crash always names both its package and its exception.
ALTER TABLE flow_executions
    ADD COLUMN crash_package text,
    ADD COLUMN crash_exception text,
    ADD CONSTRAINT flow_crash_both_or_neither CHECK ((crash_package IS NULL) = (crash_exception IS NULL));
