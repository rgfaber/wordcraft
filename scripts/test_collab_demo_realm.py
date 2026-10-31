#!/usr/bin/env python3
"""The driver joins one realm, and never the production one.

Run:  python3 -m unittest discover -s scripts -p 'test_*.py'

Proves macula-craft-collab#41's driver half: the run's realm comes from the
sweep (not from a constant), `io.macula` is refused outright with no override,
a run that names no realm does not start rather than defaulting, and the
auto-join every instance receives is the run's own realm — so the realm the
app signs in to and the realm it silently joins cannot disagree.
"""

import os
import sys
import unittest
from types import SimpleNamespace

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import collab_demo


class SweepRealmTest(unittest.TestCase):
    def test_a_sweep_naming_the_production_realm_is_refused(self):
        with self.assertRaises(collab_demo.RealmRefused) as refused:
            collab_demo.sweep_realm({"MACULA_SWEEP_REALM": "io.macula"})
        self.assertIn("io.macula", str(refused.exception), "the refusal names the realm it refused")

    def test_production_is_refused_however_it_is_named(self):
        with self.assertRaises(collab_demo.RealmRefused):
            collab_demo.sweep_realm({"MACULA_CRAFT_AUTOJOIN_REALM": "io.macula"})
        with self.assertRaises(collab_demo.RealmRefused):
            collab_demo.sweep_realm({"MACULA_SWEEP_REALM": " io.macula "})
        with self.assertRaises(collab_demo.RealmRefused):
            collab_demo.sweep_realm({"MACULA_SWEEP_REALM": "io.macula", "MACULA_CRAFT_AUTOJOIN_REALM": "io.macula"})

    def test_a_run_that_names_no_realm_does_not_start(self):
        for environ in ({}, {"MACULA_SWEEP_REALM": ""}, {"MACULA_SWEEP_REALM": "   "}):
            with self.assertRaises(collab_demo.RealmRefused, msg=f"no default: {environ}"):
                collab_demo.sweep_realm(environ)

    def test_the_sweeps_realm_is_the_one_the_run_joins(self):
        self.assertEqual(collab_demo.sweep_realm({"MACULA_SWEEP_REALM": "net.beam-campus"}), "net.beam-campus")
        self.assertEqual(
            collab_demo.sweep_realm({"MACULA_CRAFT_AUTOJOIN_REALM": "net.beam-campus"}),
            "net.beam-campus",
            "a harness that exported only the auto-join variable names the same one realm",
        )

    def test_two_variables_that_disagree_are_refused(self):
        with self.assertRaises(collab_demo.RealmRefused):
            collab_demo.sweep_realm({"MACULA_SWEEP_REALM": "net.beam-campus", "MACULA_CRAFT_AUTOJOIN_REALM": "io.macula"})


class AutoJoinEnvTest(unittest.TestCase):
    def demo(self, realm_name):
        stand_in = collab_demo.Demo.__new__(collab_demo.Demo)
        stand_in.args = SimpleNamespace(realm_name=realm_name)
        return stand_in

    def test_every_instance_is_given_the_runs_own_realm(self):
        self.assertEqual(self.demo("net.beam-campus").autojoin_env(), {"MACULA_CRAFT_AUTOJOIN_REALM": "net.beam-campus"})

    def test_the_auto_join_cannot_name_another_realm(self):
        # The value is the run's slice of args and nothing else, so there is no
        # second constant to drift: a demo configured for one realm auto-joins
        # that realm.
        for realm in ("net.beam-campus", "io.macula"):
            self.assertEqual(set(self.demo(realm).autojoin_env()), {"MACULA_CRAFT_AUTOJOIN_REALM"})
            self.assertEqual(self.demo(realm).autojoin_env()["MACULA_CRAFT_AUTOJOIN_REALM"], realm)


if __name__ == "__main__":
    unittest.main()
