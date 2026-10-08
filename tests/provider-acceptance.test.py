import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("acceptance", ROOT / "scripts/provider-acceptance.py")
acceptance = importlib.util.module_from_spec(spec)
spec.loader.exec_module(acceptance)


class AcceptanceTests(unittest.TestCase):
    def test_no_opt_in_means_no_file_read_or_network(self):
        args = ["acceptance", "--origin", "https://gateway.invalid", "--model", "test", "--key-file", "/does-not-exist"]
        with patch("sys.argv", args), patch.object(acceptance.urllib.request, "build_opener") as network, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                acceptance.main()
            network.assert_not_called()

    def test_protocol_bodies_are_bounded_and_stateless(self):
        for protocol, path, cap in [("chat", "/v1/chat/completions", "max_completion_tokens"), ("responses", "/v1/responses", "max_output_tokens"), ("messages", "/v1/messages", "max_tokens")]:
            endpoint, body = acceptance.request_body(protocol, "chosen")
            self.assertEqual(endpoint, path)
            self.assertEqual(body[cap], 8)
            self.assertFalse(body["stream"])
            self.assertEqual(body["model"], "chosen")
            if protocol == "responses":
                self.assertFalse(body["store"])

    def test_invalid_origins_are_rejected_before_network(self):
        for origin in ["http://gateway.invalid", "https://user:pass@gateway.invalid", "https://gateway.invalid/path", "https://gateway.invalid?secret=x", "https://gateway.invalid#fragment"]:
            args = ["acceptance", "--origin", origin, "--model", "test", "--key-file", "/does-not-exist", "--allow-paid-request"]
            with patch("sys.argv", args), patch.object(acceptance.urllib.request, "build_opener") as network, contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    acceptance.main()
                network.assert_not_called()

    def test_redirects_never_forward_authorization(self):
        self.assertIsNone(acceptance.NoRedirect().redirect_request(None, None, 302, "redirect", {}, "https://elsewhere.invalid"))

    def test_one_mock_request_redacts_response_and_credential(self):
        with tempfile.TemporaryDirectory() as folder:
            key = Path(folder) / "key"
            key.write_text("fixture-secret-not-real\n")
            key.chmod(0o600)
            args = ["acceptance", "--origin", "https://gateway.invalid", "--model", "chosen", "--key-file", str(key), "--protocol", "responses", "--allow-paid-request"]
            output = io.StringIO()
            with patch("sys.argv", args), patch.object(acceptance.urllib.request, "build_opener") as network, contextlib.redirect_stdout(output):
                response = network.return_value.open.return_value.__enter__.return_value
                response.status = 200
                response.read.return_value = json.dumps({"usage": {}, "output_text": "private fixture response"}).encode()
                self.assertEqual(acceptance.main(), 0)
                network.return_value.open.assert_called_once()
                request = network.return_value.open.call_args.args[0]
                self.assertEqual(request.full_url, "https://gateway.invalid/v1/responses")
                self.assertEqual(request.get_header("Authorization"), "Bearer fixture-secret-not-real")
            self.assertNotIn("fixture-secret", output.getvalue())
            self.assertNotIn("private fixture response", output.getvalue())
            self.assertTrue(json.loads(output.getvalue())["response_body_redacted"])


if __name__ == "__main__":
    unittest.main()
