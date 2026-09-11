-- A forward_pending row (status_code 2) persists its resolved adjacency in
-- status_param3. A row without one predates that: its queue assignment
-- cannot be recovered (the poll drops it as garbage and the restart
-- consistency sweep would destroy the bundle), so re-park it to waiting
-- (status_code 1) for the next dispatch pass to re-route.
UPDATE bundles SET status_code = 1, status_param1 = NULL, status_param2 = NULL
    WHERE status_code = 2 AND status_param3 IS NULL;
