"""Offline regression tests: no Forge binary, model endpoint, or dependency needed."""

import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import MagicMock, patch

import mcp_comparison as comparison


class FakeTransport:
    def __init__(self, results):
        self.results = iter(results)
        self.calls = []

    def request(self, method, params):
        self.calls.append((method, params))
        result = next(self.results)
        if isinstance(result, Exception):
            raise result
        return {"structuredContent": result}


class FakeModel:
    def __init__(self, responses):
        self.responses = iter(responses)
        self.requests = []

    def complete(self, messages, tools):
        self.requests.append((list(messages), tools))
        response = next(self.responses)
        if isinstance(response, Exception):
            raise response
        return response


def response(content="done", calls=None, usage=None):
    result = {"choices": [{"message": {"content": content, "tool_calls": calls},
                           "finish_reason": "tool_calls" if calls else "stop"}]}
    if usage is not None:
        result["usage"] = usage
    return result


def tool_call(name, arguments):
    return {"id": "call_1", "type": "function",
            "function": {"name": name, "arguments": json.dumps(arguments)}}


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.project = comparison.fixture(self.root, "hidden-marker")

    def session(self, results):
        return comparison.Session(FakeTransport(results), self.project)

    def test_model_selects_advertised_tool_and_receives_instructions(self):
        session = self.session([{"instructions": "hidden-marker"}])
        model = FakeModel([
            response(calls=[tool_call("forge_skill_show", {"name": "reviewing"})],
                     usage={"prompt_tokens": 11, "completion_tokens": 4}),
            response("hidden-marker", usage={"prompt_tokens": 19, "completion_tokens": 3}),
        ])
        usage = comparison.Usage()
        final = comparison.model_agent(
            session, model, [{"name": "forge_skill_show", "inputSchema": {"type": "object"}}],
            "Server-specific instructions", comparison.TASKS["skill"], 3, usage)
        comparison.verify("skill", session, final, "hidden-marker")
        self.assertEqual(usage.report()["input_tokens"], 30)
        self.assertEqual(usage.report()["output_tokens"], 7)
        self.assertIn("Server-specific instructions", model.requests[0][0][0]["content"])
        self.assertEqual(session.transport.calls[0][1]["name"], "forge_skill_show")

    def test_partial_usage_and_request_failure_do_not_imply_complete_totals(self):
        for last in (response(), comparison.Failure("model_http", "safe failure")):
            with self.subTest(last=type(last).__name__):
                session = self.session([{}])
                model = FakeModel([
                    response(calls=[tool_call("anything", {})],
                             usage={"prompt_tokens": 10, "completion_tokens": 2}), last])
                usage = comparison.Usage()
                try:
                    comparison.model_agent(
                        session, model, [{"name": "anything", "inputSchema": {}}],
                        "", "task", 3, usage)
                except comparison.Failure:
                    pass
                self.assertEqual(usage.calls, 2)
                self.assertIsNone(usage.report()["input_tokens"])
                self.assertIsNone(usage.report()["output_tokens"])
                self.assertEqual(usage.report()["usage_reported_calls"]["input_tokens"], 1)

    def test_zero_calls_and_missing_output_usage_are_unknown(self):
        usage = comparison.Usage()
        self.assertIsNone(usage.report()["input_tokens"])
        usage.calls = 1
        usage.record({"usage": {"prompt_tokens": 0}})
        self.assertEqual(usage.report()["input_tokens"], 0)
        self.assertIsNone(usage.report()["output_tokens"])

    def test_wrong_answer_and_self_report_are_rejected(self):
        session = self.session([{"instructions": "hidden-marker"}])
        session.call("forge_skill_show", {"name": "reviewing"})
        with self.assertRaisesRegex(comparison.Failure, "Final answer"):
            comparison.verify("skill", session, "wrong marker", "hidden-marker")
        with self.assertRaisesRegex(comparison.Failure, "graph result"):
            comparison.verify("lookup", session, "authenticate_session auth_policy.rs", "")
        session = self.session([{"hits": [{"path": "auth_policy.rs",
                                         "reasons": ["symbols match: authenticate_session"]}]}])
        session.call("forge_graph_context", {"query": "authenticate_session"})
        for final in ("unrelated.rs", "authenticate_session wrong_auth_policy.rs",
                      "authenticate_session auth_policy.rs.backup",
                      "authenticate_session wrong/auth_policy.rs",
                      "not_authenticate_session auth_policy.rs"):
            with self.subTest(final=final):
                with self.assertRaisesRegex(comparison.Failure, "Final answer"):
                    comparison.verify("lookup", session, final, "")
        comparison.verify("lookup", session, "authenticate_session auth_policy.rs", "")

    def test_lookup_rejects_echoed_input_and_requires_matching_evidence_in_one_hit(self):
        for tool, data in [
            ("forge_graph_context", {"query": "authenticate_session auth_policy.rs", "hits": []}),
            ("forge_graph_grep", {"pattern": "authenticate_session auth_policy.rs", "matches": []}),
            ("forge_graph_context", {"hits": [
                {"path": "auth_policy.rs", "reasons": ["path matches auth"]},
                {"path": "unrelated.rs", "reasons": ["symbols match authenticate_session"]},
            ]}),
            ("forge_graph_grep", {"matches": [
                {"file": "auth_policy.rs", "text": "pub fn other() {}"},
                {"file": "unrelated.rs", "text": "pub fn authenticate_session() {}"},
            ]}),
        ]:
            with self.subTest(tool=tool, data=data):
                session = self.session([data])
                session.call(tool, {})
                with self.assertRaisesRegex(comparison.Failure, "graph result"):
                    comparison.verify("lookup", session, "authenticate_session auth_policy.rs", "")
        session = self.session([{"matches": [
            {"file": "auth_policy.rs", "line": 1, "text": "pub fn authenticate_session() {}"},
        ]}])
        session.call("forge_graph_grep", {"pattern": "authenticate_session"})
        comparison.verify("lookup", session, "authenticate_session auth_policy.rs", "")

    def test_model_tool_result_uses_one_representation_and_preserves_errors(self):
        data = {"instructions": "unique-marker"}
        content = [{"type": "text", "text": json.dumps(data)}]
        result = {"structuredContent": data, "content": content, "isError": True}
        rendered = comparison.model_tool_result(result)
        self.assertEqual(rendered, {"data": data, "isError": True})
        self.assertEqual(comparison.encode(rendered).count("unique-marker"), 1)
        self.assertEqual(comparison.model_tool_result({"content": content}),
                         {"content": content, "isError": False})

    def approval_session(self, approved, parked=True):
        session = self.session([
            {"run_id": "r1", "status": "waiting_for_approval" if parked else "running"},
            {"delivered": True}, {"run_id": "r1", "status": "completed"},
        ])
        session.call("forge_tools_invoke", {"name": "forge_run", "arguments": {"prompt": "write"}})
        session.call("forge_tools_invoke", {
            "name": "forge_run_input", "arguments": {"run_id": "r1", "input": "y" if approved else "n"},
        })
        if approved:
            (self.project / "notes.txt").write_text("scripted content")
        session.call("forge_tools_invoke", {
            "name": "forge_run_status", "arguments": {"run_id": "r1"},
        })
        logs = self.project / ".forge/sessions"
        logs.mkdir(exist_ok=True)
        (logs / "session.jsonl").write_text(json.dumps({
            "run_id": "r1", "type": "tool_completed", "name": "write_file", "success": approved,
            "message": "written" if approved else "approval denied",
        }) + "\n")
        return session

    def test_approval_and_denial_need_park_decision_completion_and_file_evidence(self):
        for approved in (False, True):
            task = "approve" if approved else "deny"
            session = self.approval_session(approved)
            comparison.verify(task, session, "done", "")
            self.assertEqual(session.trace[0]["native"], "forge_run")
            session.trace.pop(1)
            with self.assertRaisesRegex(comparison.Failure, "delivered"):
                comparison.verify(task, session, "done", "")

    def test_denial_cannot_pass_by_doing_nothing(self):
        with self.assertRaisesRegex(comparison.Failure, "delegated"):
            comparison.verify("deny", self.session([]), "I denied the write", "")

    def test_decision_without_observed_pause_is_rejected(self):
        session = self.approval_session(False, parked=False)
        with self.assertRaisesRegex(comparison.Failure, "parked"):
            comparison.verify("deny", session, "done", "")

    def test_environment_drops_provider_keys_and_forge_overrides(self):
        with patch.dict(os.environ, {"FORGE_MODEL": "remote", "ODD_PROVIDER_TOKEN": "secret",
                                     "OPENAI_API_KEY": "secret", "FORGE_JEV_URL": "remote"}):
            env = comparison.isolated_env(self.root)
        self.assertNotIn("ODD_PROVIDER_TOKEN", env)
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertNotIn("FORGE_MODEL", env)
        self.assertNotIn("FORGE_JEV_URL", env)
        self.assertEqual(env["FORGE_TEST_MOCKS"], "1")

    def test_cli_and_privacy_guards(self):
        for arguments in (["--trials", "0"], ["--max-turns", "-1"], ["--timeout", "nan"],
                          ["--timeout", "inf"], ["--timeout", "0"],
                          ["--mode", "model"], ["--base-url", "http://localhost/v1"]):
            with self.subTest(arguments=arguments), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    comparison.parse_args(arguments)
        for url in ("http://localhost:8080/v1", "http://127.0.0.1/v1", "http://[::1]/v1"):
            comparison.validate_url(url, False)
        for url in ("https://provider.example/v1", "http://localhost.example/v1",
                    "file:///v1", "http://secret@localhost/v1", "http://localhost/v1?key=x",
                    "http://localhost:99999/v1", "http://localhost/v2"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                comparison.validate_url(url, False)
        comparison.validate_url("https://provider.example/v1", True)
        with self.assertRaisesRegex(ValueError, "HTTPS"):
            comparison.validate_url("http://provider.example/v1", True, has_key=True)
        comparison.validate_url("http://localhost/v1", False, has_key=True)

    def test_http_errors_and_redirects_do_not_expose_body_or_credentials(self):
        model = comparison.Model("http://localhost/v1", "test", "SECRET", 1)
        for status in (401, 302, 307):
            connection = MagicMock()
            connection.getresponse.return_value.status = status
            connection.getresponse.return_value.read.return_value = b"SECRET response"
            with patch.object(comparison.http.client, "HTTPConnection", return_value=connection):
                with self.assertRaises(comparison.Failure) as caught:
                    model.complete([], [])
            self.assertEqual(str(caught.exception),
                             f"Model HTTP request failed (status {status})")
            connection.request.assert_called_once()
            connection.getresponse.return_value.read.assert_not_called()

    def test_http_wall_deadline(self):
        model = comparison.Model("http://localhost/v1", "test", None, 0.05)
        connection = MagicMock()
        connection.getresponse.side_effect = lambda: time.sleep(0.3)
        with patch.object(comparison.http.client, "HTTPConnection", return_value=connection):
            started = time.monotonic()
            with self.assertRaisesRegex(comparison.Failure, "exceeded"):
                model.complete([], [])
            self.assertLess(time.monotonic() - started, 0.2)
        connection.sock.shutdown.assert_called_once()

    def test_subprocess_notifications_ids_stderr_and_timeout_cleanup(self):
        script = (
            "import sys,json,time\n"
            "request=json.loads(sys.stdin.readline())\n"
            "sys.stderr.write('secret'*50000);sys.stderr.flush()\n"
            "print(json.dumps({'method':'notifications/test'}),flush=True)\n"
            "print(json.dumps({'id':999,'result':{}}),flush=True)\n"
            "print(json.dumps({'id':request['id'],'result':{'ok':True}}),flush=True)\n"
            "time.sleep(30)\n"
        )
        client = comparison.MCP([sys.executable, "-c", script], self.root,
                                comparison.isolated_env(self.root), 1)
        self.addCleanup(client.close)
        self.assertEqual(client.request("test", {}), {"ok": True})
        started = time.monotonic()
        with self.assertRaisesRegex(comparison.Failure, "exceeded"):
            client.request("hang", {})
        self.assertIsNotNone(client.child.poll(), "timed-out Forge child must be reaped")
        self.assertLess(time.monotonic() - started, 5)


if __name__ == "__main__":
    unittest.main()
