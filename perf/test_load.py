#!/usr/bin/env python3
"""load.py exit-code contract: a scenario that declares a failed check
must make the process exit non-zero — CI relies on it (a `passed:false`
artifact used to land under a green workflow). Run with:
    python3 -m unittest perf.test_load   (from the repo root), or
    python3 perf/test_load.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import load  # noqa: E402


def _stub(value, sink=None, name=None):
    def run():
        if sink is not None:
            sink.append(name)
        return value
    return run


class MainExitCode(unittest.TestCase):
    def test_all_scenarios_pass_exits_zero(self):
        rc = load.main(
            which="all",
            scenarios={"a": _stub(True), "b": _stub(True)},
            check_bins=False,
        )
        self.assertEqual(rc, 0)

    def test_a_failed_scenario_exits_one(self):
        rc = load.main(
            which="all",
            scenarios={"a": _stub(True), "b": _stub(False)},
            check_bins=False,
        )
        self.assertEqual(rc, 1)

    def test_every_scenario_still_runs_after_a_failure(self):
        ran = []
        rc = load.main(
            which="all",
            scenarios={
                "bad": _stub(False, ran, "bad"),
                "good": _stub(True, ran, "good"),
            },
            check_bins=False,
        )
        self.assertEqual(rc, 1)
        self.assertEqual(ran, ["bad", "good"])

    def test_named_scenario_runs_only_that_one(self):
        ran = []
        rc = load.main(
            which="good",
            scenarios={"good": _stub(True, ran, "good"),
                       "bad": _stub(False, ran, "bad")},
            check_bins=False,
        )
        self.assertEqual(rc, 0)
        self.assertEqual(ran, ["good"])

    def test_unknown_scenario_exits_nonzero(self):
        with self.assertRaises(SystemExit) as ctx:
            load.main(which="nope", scenarios={"a": _stub(True)}, check_bins=False)
        self.assertNotEqual(ctx.exception.code, 0)


if __name__ == "__main__":
    unittest.main()
