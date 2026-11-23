import json
import os
import re
import stat
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dashboard


CJK_RE = re.compile(r"[\u4e00-\u9fff]")


class TestDashboardSecurityAndConfig(unittest.TestCase):
    def setUp(self) -> None:
        self.repo_root = Path(__file__).resolve().parent.parent

    def test_example_pyrite_json_validates(self) -> None:
        cfg_path = self.repo_root / "pyrite.example.json"
        data = json.loads(cfg_path.read_text(encoding="utf-8"))
        ok, err = dashboard.validate_pyrite_config(data)
        self.assertTrue(ok, err)

    def test_validate_rejects_secrets_in_pyrite_json(self) -> None:
        for bad in (
            {"chain": "eth", "private_key": "0x" + "11" * 32},
            {"chain": "eth", "mode": {"flashbots_auth_key": "0x" + "22" * 32}},
            {"chain": "eth", "solver": {"secret": "abc"}},
        ):
            ok, err = dashboard.validate_pyrite_config(bad)
            self.assertFalse(ok)
            self.assertIn("pyrite.json", err)

    def test_write_only_wallet_and_chmod_0600(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            env_path = root / ".env"
            logs_dir = root / "logs"
            logs_dir.mkdir()

            secret_pk = "0x" + "ab" * 32
            res = dashboard.import_wallet_key(
                env_path, logs_dir, "PRIVATE_KEY", secret_pk, None, "eth"
            )
            self.assertTrue(res["ok"])
            mode = stat.S_IMODE(env_path.stat().st_mode)
            self.assertEqual(mode, 0o600)

            status = dashboard.read_wallet_status(env_path, logs_dir, None, "eth")
            self.assertTrue(status["has_private_key"])
            self.assertFalse(status["has_flashbots_auth_key"])
            self.assertTrue(status["env_mode_0600"])
            self.assertNotIn("ab" * 8, json.dumps(status))
            self.assertNotIn("ab" * 8, json.dumps(res))

            with self.assertRaises(ValueError):
                dashboard.import_wallet_key(env_path, logs_dir, "PRIVATE_KEY", "not_hex", None, "eth")

            with self.assertRaises(ValueError):
                dashboard.generate_burner_wallet(env_path, logs_dir, None, "eth", force=False)

            gen = dashboard.generate_burner_wallet(env_path, logs_dir, None, "eth", force=True)
            self.assertTrue(gen["ok"])
            self.assertTrue(gen["wallet"]["has_private_key"])
            self.assertNotIn("PRIVATE_KEY", json.dumps(gen))

    def test_submit_log_regex_matches_mev_share_and_bundle(self) -> None:
        lines = [
            '2025-01-01T00:02:02Z  INFO pyrite: SUBMITTED MEV-SHARE BACKRUN BUNDLE victim_hash=0xaa eoa=0x1111111111111111111111111111111111111111 tx_hash=0xbb strategy=DAG(2steps) gross_usd="$1.58"',
            '2025-01-01T00:01:02Z  INFO pyrite: SUBMITTED MEV BUNDLE block=20000001 eoa=0x1111111111111111111111111111111111111111 tx_hash=0xcc strategy=Cyclic(3hop) gross_usd="$0.48"',
        ]
        for ln in lines:
            self.assertIsNotNone(dashboard.SUBMIT_LOG_RE.search(ln))

    def test_sample_logs_load_summary(self) -> None:
        sample_logs = self.repo_root / "examples" / "sample_logs"
        # Skip live RPC waits in unit tests by keeping blocking_rpc_if_cold=False.
        s = dashboard.load_summary(sample_logs, "eth", blocking_rpc_if_cold=False)
        self.assertEqual(s["chain"], "eth")
        self.assertEqual(s["sampled"], 8)
        self.assertEqual(s["sim_ok_count"], 6)
        self.assertIsNotNone(s["latest_scan"])
        self.assertGreaterEqual(len(s["recent_bundles"]), 3)
        self.assertIsInstance(s["topology_mtime_ms"], int)

    def test_web_html_i18n_keys_complete_and_default_english(self) -> None:
        html = dashboard.WEB_HTML
        self.assertIn('<html lang="en">', html)
        self.assertIn('id="langSelect"', html)

        m = re.search(
            r"// I18N_START\s*const I18N = (\{.*?\});\s*// I18N_END",
            html,
            re.DOTALL,
        )
        self.assertIsNotNone(m, "Expected I18N JSON block delimited by I18N_START / I18N_END")
        assert m is not None
        i18n = json.loads(m.group(1))
        self.assertEqual(set(i18n.keys()), {"en", "zh"})
        en_keys = set(i18n["en"].keys())
        zh_keys = set(i18n["zh"].keys())
        self.assertEqual(en_keys, zh_keys)

        referenced = set(re.findall(r'data-i18n(?:-(?:placeholder|html))?="([^"]+)"', html))
        referenced.update(re.findall(r"\bt\(\s*['\"]([A-Za-z0-9_]+)['\"]", html))
        missing = sorted(referenced - en_keys)
        self.assertEqual(missing, [], f"Missing i18n keys: {missing}")

        for k, v in i18n["en"].items():
            self.assertIsNone(CJK_RE.search(v), f"Unexpected CJK character in en[{k}]")

        src = (self.repo_root / "scripts" / "dashboard.py").read_text(encoding="utf-8")
        cleaned = re.sub(
            r"// I18N_START.*?// I18N_END",
            "",
            src,
            flags=re.DOTALL,
        ).replace('<option value="zh">\u4e2d\u6587</option>', "")
        self.assertIsNone(CJK_RE.search(cleaned), "Found CJK text outside I18N block")

    def test_load_topology_multi_chain_and_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            logs_dir = Path(td)
            eth_cache = {
                "tokens": [
                    ["0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", "WETH", 18],
                    ["0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "USDC", 6],
                    ["0xdac17f958d2ee523a2206206994597c13d831ec7", "USDT", 6],
                ],
                "pools": [
                    {
                        "address": "0x1111111111111111111111111111111111111111",
                        "factory_name": "UniV3",
                        "token0": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                        "token1": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    },
                    {
                        "address": "0x2222222222222222222222222222222222222222",
                        "factory_name": "UniV4",
                        "token0": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                        "token1": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                    },
                ],
            }
            base_cache = {
                "tokens": [
                    ["0x4200000000000000000000000000000000000006", "WETH", 18],
                    ["0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", "USDC", 6],
                    ["0x940181a94a35a4569e4529a3cdfb74e38fd98631", "AERO", 18],
                ],
                "pools": [
                    {
                        "address": "0x3333333333333333333333333333333333333333",
                        "factory_name": "AerodromeV2",
                        "token0": "0x4200000000000000000000000000000000000006",
                        "token1": "0x940181a94a35a4569e4529a3cdfb74e38fd98631",
                    },
                    {
                        "address": "0x4444444444444444444444444444444444444444",
                        "factory_name": "AerodromeCL",
                        "token0": "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                        "token1": "0x940181a94a35a4569e4529a3cdfb74e38fd98631",
                    },
                ],
            }
            (logs_dir / "pools_cache_eth.json").write_text(json.dumps(eth_cache), encoding="utf-8")
            (logs_dir / "pools_cache_base.json").write_text(json.dumps(base_cache), encoding="utf-8")

            eth_topo = dashboard.load_topology(logs_dir, "eth")
            self.assertEqual(eth_topo["source"], "pools_cache")
            self.assertEqual(eth_topo["pool_count"], 2)
            self.assertEqual(eth_topo["tokens"][eth_topo["hubs"][0]], "WETH")
            self.assertEqual(dashboard.load_topology(logs_dir, "eth"), eth_topo)

            base_topo = dashboard.load_topology(logs_dir, "base")
            self.assertEqual(base_topo["source"], "pools_cache")
            self.assertEqual(base_topo["tokens"][base_topo["hubs"][0]], "AERO")

        sample_logs = self.repo_root / "examples" / "sample_logs"
        fb_topo = dashboard.load_topology(sample_logs, "eth")
        self.assertEqual(fb_topo["source"], "opportunities_fallback")
        self.assertGreater(fb_topo["pool_count"], 0)
        self.assertEqual(fb_topo["tokens"][fb_topo["hubs"][0]], "WETH")

    def test_web_html_topology_tab_and_url_routes(self) -> None:
        html = dashboard.WEB_HTML
        for token in (
            'data-tab="topology"',
            'id="tab-topology"',
            'id="topoCanvas"',
            "/topology",
            "/monitor",
            "popstate",
            "pushState",
            "/api/topology",
            "state.recent",
            "TOPO_FAIL_TTL_MS",
            "TOPO_OK_TTL_MS",
            "enqueueTopoPulses",
            "topoPulses",
            "topology_mtime_ms",
            "topoHubSlotsByChain",
            "topoHubSeat",
            "topoNewPoolPings",
            "routeSigOf",
            "seedDistinctTopoPulses",
        ):
            self.assertIn(token, html)
        for forbidden in (
            "bsc" "-pools.json",
            "UTC" "+8",
            "recent_opps",
            "idx * 2.399963",
            "hash01(addr, idx",
        ):
            self.assertNotIn(forbidden, html)

    def test_topology_strips_legacy_more_steps_suffix(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            logs_dir = Path(tmp)
            (logs_dir / "opportunities.jsonl").write_text(
                json.dumps(
                    {
                        "chain": "bsc",
                        "route": "PancakeV3:WBNB->USDT | PancakeV2:HDF->WBNB (+4 more, 12 steps)",
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            topo = dashboard.load_topology(logs_dir, "bsc")
            self.assertIn("WBNB", topo["tokens"])
            self.assertIn("HDF", topo["tokens"])
            for tok in topo["tokens"]:
                self.assertNotIn("more", tok.lower())
                self.assertNotIn("steps", tok.lower())
        self.assertIn(r"(?:,\s*\d+\s+steps)?", dashboard.WEB_HTML)


if __name__ == "__main__":
    unittest.main()
