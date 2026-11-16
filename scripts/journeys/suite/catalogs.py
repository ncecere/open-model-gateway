"""Model selection, catalogues and type defaults.

Everything is done on throwaway objects (a workspace, a catalogue) or as a no-op: the live type
defaults and catalogue assignments are only ever written back unchanged, and restored if that
changed anything."""
from ..registry import journey
from .common import NO_SUCH_ID, rows

TYPE_POLICY_FIELDS = ("requests_per_minute", "tokens_per_minute", "concurrent_requests", "budgets", "concurrent_jobs", "storage_bytes")


@journey("catalogs-models")
def catalogs_models(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    base = f"/api/v1/workspaces/{ws}"
    platform = f"/api/v1/platform/workspaces/{ws}"

    available = rows(alex.get(f"{base}/available-models")) or []
    choices = [m for m in available if m.get("model_id")]
    if len(choices) < 2:
        t.rec.fail("models.environment-has-two-models", "setup_failed")
        return
    first, second = choices[0]["model_id"], choices[1]["model_id"]

    # Workspace model selection by the workspace admin: select, list, compare, deselect.
    t.expect("selection.anonymous-refused", anon.get(f"{base}/models"), 401)
    t.expect("selection.list-starts-empty", alex.get(f"{base}/models"), 200)
    t.ok("selection.nothing-selected-at-first", rows(alex.get(f"{base}/models")) == [])
    t.expect("selection.member-cannot-select", blair.post(f"{base}/models", {"model_id": first}), 403)
    t.ok("selection.refused-select-left-nothing", rows(alex.get(f"{base}/models")) == [])
    t.expect("selection.select", alex.post(f"{base}/models", {"model_id": first}), 200)
    t.undo("deselect model", lambda: alex.delete(f"{base}/models/{first}"), gone=(404,))
    t.ok("selection.selected-is-listed", any(m.get("model_id") == first for m in rows(alex.get(f"{base}/models")) or []))
    t.expect("selection.unknown-model-refused", alex.post(f"{base}/models", {"model_id": NO_SUCH_ID}), 400)
    t.expect("selection.member-cannot-deselect", blair.delete(f"{base}/models/{first}"), 403)
    compare = alex.get(f"{base}/models/compare?ids={first},{second}")
    t.expect("selection.compare", compare, 200)
    t.ok("selection.compare-shape", compare.get("activity") == "workspace" and len(rows(compare) or []) == 2 and all(isinstance((m.get("metrics") or {}).get("requests"), str) for m in rows(compare) or []))
    t.expect("selection.compare-needs-ids", alex.get(f"{base}/models/compare"), 400)
    t.expect("selection.compare-by-member", blair.get(f"{base}/models/compare?ids={first},{second}"), 200)
    t.expect("selection.compare-anonymous-refused", anon.get(f"{base}/models/compare?ids={first},{second}"), 401)
    t.expect("selection.deselect", alex.delete(f"{base}/models/{first}"), 200)
    t.ok("selection.deselected-is-gone", rows(alex.get(f"{base}/models")) == [])

    # Direct platform assignment, independent of the catalogue.
    t.expect("assignment.member-cannot-assign", alex.post(f"{platform}/models", {"model_id": first}), 403)
    t.expect("assignment.auditor-cannot-assign", auditor.post(f"{platform}/models", {"model_id": first}), 403)
    t.expect("assignment.assign", operator.post(f"{platform}/models", {"model_id": first}), 200)
    t.undo("unassign model", lambda: operator.delete(f"{platform}/models/{first}"), gone=(404,))
    t.ok("assignment.assigned-is-direct", any(m.get("model_id") == first and m.get("direct_granted") is True for m in rows(alex.get(f"{base}/available-models")) or []))
    t.expect("assignment.unassign", operator.delete(f"{platform}/models/{first}"), 200)
    t.expect("platform.models-compare", operator.get(f"/api/v1/platform/models/compare?ids={first},{second}"), 200)
    t.expect("platform.models-compare-member-refused", alex.get(f"/api/v1/platform/models/compare?ids={first},{second}"), 403)

    # A throwaway catalogue: create, rename, give it a model, override a workspace with it, remove.
    catalogs = "/api/v1/platform/catalogs"
    t.expect("catalogs.create-by-member-refused", alex.post(catalogs, {}), 403)
    created = operator.post(catalogs, {"name": t.label("catalogue")})
    t.expect("catalogs.create", created, 200)
    cid = created.id()
    if cid:
        t.undo("remove catalogue", lambda: operator.delete(f"{catalogs}/{cid}"), gone=(404,))
        t.expect("catalogs.read", operator.get(f"{catalogs}/{cid}"), 200)
        t.expect("catalogs.rename", operator.patch(f"{catalogs}/{cid}", {"name": t.label("renamed")}), 200)
        t.expect("catalogs.set-models", operator.put(f"{catalogs}/{cid}/models", {"model_ids": [first]}), 200)
        t.ok("catalogs.models-listed", [m.get("id") for m in rows(operator.get(f"{catalogs}/{cid}/models")) or []] == [first])
        t.expect("catalogs.model-lists-it", operator.get(f"/api/v1/platform/models/{first}/catalogs"), 200)
        t.expect("catalogs.override-by-member-refused", alex.put(f"{platform}/catalogs", {"mode": "replace", "catalog_ids": [cid]}), 403)
        t.expect("catalogs.override-workspace", operator.put(f"{platform}/catalogs", {"mode": "replace", "catalog_ids": [cid]}), 200)
        t.undo("clear workspace catalogue override", lambda: operator.delete(f"{platform}/catalogs"))
        override = operator.get(f"{platform}/catalogs")
        t.ok("catalogs.override-applies", override.get("mode") == "replace" and override.get("effective_catalog_ids") == [cid])
        t.ok("catalogs.workspace-sees-only-its-catalogue", [m.get("model_id") for m in rows(alex.get(f"{base}/available-models")) or []] == [first])
        t.expect("catalogs.clear-override", operator.delete(f"{platform}/catalogs"), 200)
        t.ok("catalogs.override-cleared", operator.get(f"{platform}/catalogs").get("mode") == "inherit")
        t.expect("catalogs.remove", operator.delete(f"{catalogs}/{cid}"), 200)
        t.expect("catalogs.removed-is-gone", operator.get(f"{catalogs}/{cid}"), 404)

    # Live type defaults: read, write the same values back, read again. The original is restored if that differs.
    for kind in ("team", "project"):
        url = f"/api/v1/platform/workspace-types/{kind}/policy"
        original = operator.get(url)
        policy = original.get("policy")
        body = {k: policy[k] for k in TYPE_POLICY_FIELDS if isinstance(policy, dict) and k in policy}
        t.expect(f"type-{kind}.policy-read", original, 200)
        t.expect(f"type-{kind}.policy-write-by-member-refused", alex.put(url, body), 403)
        t.expect(f"type-{kind}.policy-write-by-auditor-refused", auditor.put(url, body), 403)
        if body:
            t.undo(f"restore {kind} type policy", lambda u=url, b=body: operator.put(u, b))
            t.expect(f"type-{kind}.policy-unchanged-write", operator.put(url, body), 200)
            t.ok(f"type-{kind}.policy-unchanged-afterwards", operator.get(url).json() == original.json())
        else:
            t.rec.fail(f"type-{kind}.policy-body-readable", "setup_failed")
        cat_url = f"/api/v1/platform/workspace-types/{kind}/catalogs"
        cats = operator.get(cat_url)
        t.expect(f"type-{kind}.catalogs-read", cats, 200)
        ids = cats.get("catalog_ids")
        t.expect(f"type-{kind}.catalogs-write-by-member-refused", alex.put(cat_url, {"catalog_ids": ids or []}), 403)
        if isinstance(ids, list):
            t.undo(f"restore {kind} type catalogues", lambda u=cat_url, i=ids: operator.put(u, {"catalog_ids": i}))
            t.expect(f"type-{kind}.catalogs-unchanged-write", operator.put(cat_url, {"catalog_ids": ids}), 200)
            t.ok(f"type-{kind}.catalogs-unchanged-afterwards", operator.get(cat_url).json() == cats.json())
    defaults = operator.get("/api/v1/platform/catalog-defaults")
    t.ok("type-defaults.listed-for-every-kind", set(defaults.json() or {}) == {"personal", "team", "project"})
    t.expect("type-defaults.write-by-member-refused", alex.put("/api/v1/platform/catalog-defaults", defaults.json() or {}), 403)
    t.expect("type-defaults.write-by-auditor-refused", auditor.put("/api/v1/platform/catalog-defaults", defaults.json() or {}), 403)
    t.ok("type-defaults.unchanged-after-refusals", operator.get("/api/v1/platform/catalog-defaults").json() == defaults.json())

    # Model setup creates connections and models: only the refusals, with a body nothing accepts.
    t.expect("model-setup.member-refused", alex.post("/api/v1/platform/model-setup", {}), 403)
    t.expect("model-setup.invalid-request-refused", operator.post("/api/v1/platform/model-setup", {}), 422)
