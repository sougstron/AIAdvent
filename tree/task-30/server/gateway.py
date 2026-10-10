#!/usr/bin/env python3
"""Single-flight CPU inference; only the LAN gateway has a published port."""
import hmac
import json
import math
import os
import signal
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LLM_MODEL = "qwen3:1.7b"
EMBED_MODEL = "nomic-embed-text"
BODY_LIMIT = 1024 * 1024
RESPONSE_LIMIT = 8 * 1024 * 1024
BACKEND = "http://ollama:11434"
INFERENCE = threading.Lock()
KEYS = {"llm": os.environ["LLM_KEY"], "embed": os.environ["EMBED_KEY"]}
if any(len(key) != 64 or any(c not in "0123456789abcdef" for c in key) for key in KEYS.values()):
    raise RuntimeError("Keys must be 64 lowercase hexadecimal characters")
if hmac.compare_digest(KEYS["llm"], KEYS["embed"]):
    raise RuntimeError("Chat and embedding keys must differ")


class RequestError(Exception):
    def __init__(self, status, message):
        self.status, self.message = status, message


def backend(path, payload):
    request = urllib.request.Request(BACKEND + path, json.dumps(payload).encode(),
                                     {"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=180) as response:
            raw = response.read(RESPONSE_LIMIT + 1)
        if len(raw) > RESPONSE_LIMIT:
            raise RequestError(502, "Backend response exceeds limit")
        result = json.loads(raw)
        if not isinstance(result, dict):
            raise ValueError("Expected backend object")
        return result
    except urllib.error.HTTPError as error:
        raw = error.read(RESPONSE_LIMIT)
        try:
            detail = json.loads(raw).get("error", "Backend rejected request")
        except (ValueError, AttributeError):
            detail = raw.decode(errors="replace")[:4096]
        raise RequestError(error.code, str(detail)) from error
    except (TimeoutError, OSError, ValueError) as error:
        raise RequestError(504 if isinstance(error, TimeoutError) else 502,
                           "Backend unavailable or invalid response: " + str(error)) from error


def tool_calls(calls, known_calls):
    if not isinstance(calls, list) or not 1 <= len(calls) <= 64:
        raise RequestError(400, "tool_calls must contain 1..64 calls")
    converted = []
    for call in calls:
        if not isinstance(call, dict) or call.get("type") != "function":
            raise RequestError(400, "Only function tool calls are supported")
        function = call.get("function")
        call_id = call.get("id")
        if (not isinstance(function, dict) or not isinstance(function.get("name"), str)
                or not function["name"] or not isinstance(call_id, str) or not call_id
                or call_id in known_calls or not isinstance(function.get("arguments"), str)):
            raise RequestError(400, "Tool calls require unique IDs, names and JSON argument strings")
        try:
            arguments = json.loads(function["arguments"])
        except (ValueError, RecursionError):
            raise RequestError(400, "Tool arguments must be JSON objects")
        if not isinstance(arguments, dict):
            raise RequestError(400, "Tool arguments must be JSON objects")
        known_calls[call_id] = function["name"]
        converted.append({"function": {"name": function["name"], "arguments": arguments}})
    return converted


def chat_messages(messages):
    if not isinstance(messages, list) or not 1 <= len(messages) <= 128:
        raise RequestError(400, "messages must contain 1..128 messages")
    converted, known_calls = [], {}
    for message in messages:
        if (not isinstance(message, dict)
                or message.get("role") not in ("system", "user", "assistant", "tool")
                or set(message) - {"role", "content", "name", "tool_calls", "tool_call_id"}):
            raise RequestError(400, "Unsupported message role or fields")
        role, content = message["role"], message.get("content")
        if content is None and role == "assistant" and message.get("tool_calls"):
            content = ""
        if not isinstance(content, str):
            raise RequestError(400, "Only text message content is supported")
        item = {"role": role, "content": content}
        if "tool_calls" in message:
            if role != "assistant":
                raise RequestError(400, "Only assistant messages can carry tool_calls")
            item["tool_calls"] = tool_calls(message["tool_calls"], known_calls)
        if role == "tool":
            call_id = message.get("tool_call_id")
            if not isinstance(call_id, str) or call_id not in known_calls:
                raise RequestError(400, "Tool reply must reference an earlier assistant call")
            item["tool_name"] = known_calls.pop(call_id)
        elif "tool_call_id" in message:
            raise RequestError(400, "Only tool replies can carry tool_call_id")
        converted.append(item)
    return converted


def chat_payload(data):
    if data.get("model") != LLM_MODEL:
        raise RequestError(400, "Only model qwen3:1.7b is available")
    if type(data.get("stream", False)) is not bool:
        raise RequestError(400, "stream must be boolean")
    allowed = {"model", "messages", "stream", "stream_options", "temperature", "top_p", "top_k",
               "seed", "stop", "max_tokens", "max_completion_tokens", "tools", "tool_choice",
               "response_format"}
    unknown = set(data) - allowed
    if unknown:
        raise RequestError(400, "Unsupported chat fields: " + ", ".join(sorted(unknown)))
    messages = chat_messages(data.get("messages"))
    stream_options = data.get("stream_options", {})
    if (not isinstance(stream_options, dict) or set(stream_options) - {"include_usage"}
            or type(stream_options.get("include_usage", False)) is not bool):
        raise RequestError(400, "Only boolean stream_options.include_usage is supported")
    choice = data.get("tool_choice", "auto")
    if choice not in ("auto", "none"):
        raise RequestError(400, "tool_choice supports auto or none")
    tools = data.get("tools", [])
    if not isinstance(tools, list) or len(tools) > 64:
        raise RequestError(400, "tools must be an array of at most 64 functions")
    for tool in tools:
        if (not isinstance(tool, dict) or tool.get("type") != "function"
                or not isinstance(tool.get("function"), dict)
                or not isinstance(tool["function"].get("name"), str)
                or not tool["function"]["name"]
                or not isinstance(tool["function"].get("parameters"), dict)):
            raise RequestError(400, "Tools require a function name and JSON parameters schema")
    options = {"num_ctx": 4096, "num_predict": 1024, "num_thread": 4}
    if "max_tokens" in data and "max_completion_tokens" in data:
        raise RequestError(400, "Use only one output token limit")
    for field in ("max_tokens", "max_completion_tokens"):
        if field in data:
            value = data[field]
            if type(value) is not int or not 1 <= value <= 2048:
                raise RequestError(400, field + " must be an integer in 1..2048")
            options["num_predict"] = value
    for field, low, high in (("temperature", 0, 2), ("top_p", 0, 1)):
        if field in data:
            value = data[field]
            if type(value) not in (int, float) or not math.isfinite(value) or not low <= value <= high:
                raise RequestError(400, field + " is outside supported range")
            options[field] = value
    if "seed" in data:
        if type(data["seed"]) is not int or not -1 <= data["seed"] <= 2147483647:
            raise RequestError(400, "seed must be an integer in -1..2147483647")
        options["seed"] = data["seed"]
    if "top_k" in data:
        if type(data["top_k"]) is not int or not -1 <= data["top_k"] <= 2147483647:
            raise RequestError(400, "top_k must be an integer in -1..2147483647")
        options["top_k"] = data["top_k"]
    if "stop" in data:
        stop = data["stop"]
        stop = [stop] if isinstance(stop, str) else stop
        if not isinstance(stop, list) or not 1 <= len(stop) <= 4 or any(not isinstance(s, str) or not s or len(s) > 256 for s in stop):
            raise RequestError(400, "stop must contain 1..4 nonempty strings of at most 256 characters")
        options["stop"] = stop
    payload = {"model": LLM_MODEL, "messages": messages, "stream": False,
               "think": False, "options": options, "keep_alive": "5m"}
    if tools and choice == "auto":
        payload["tools"] = tools
    if "response_format" in data:
        fmt = data["response_format"]
        if not isinstance(fmt, dict):
            raise RequestError(400, "response_format must be an object")
        if fmt.get("type") == "json_object" and set(fmt) == {"type"}:
            payload["format"] = "json"
        elif fmt.get("type") == "json_schema" and isinstance(fmt.get("json_schema"), dict) and isinstance(fmt["json_schema"].get("schema"), dict):
            payload["format"] = fmt["json_schema"]["schema"]
        elif fmt != {"type": "text"}:
            raise RequestError(400, "response_format supports text, json_object or json_schema")
    return payload


def chat_completion(result):
    message = result.get("message")
    if not isinstance(message, dict) or not isinstance(message.get("content"), str):
        raise RequestError(502, "Backend returned an invalid chat message")
    prompt_tokens = result.get("prompt_eval_count", 0)
    completion_tokens = result.get("eval_count", 0)
    if any(type(n) is not int or n < 0 for n in (prompt_tokens, completion_tokens)):
        raise RequestError(502, "Backend returned invalid token usage")
    output = {"role": "assistant", "content": message["content"]}
    calls = message.get("tool_calls", [])
    if not isinstance(calls, list):
        raise RequestError(502, "Backend returned invalid tool calls")
    if calls:
        output["tool_calls"] = []
        nonce = time.time_ns()
        for index, call in enumerate(calls):
            function = call.get("function") if isinstance(call, dict) else None
            if (not isinstance(function, dict) or not isinstance(function.get("name"), str)
                    or not isinstance(function.get("arguments"), dict)):
                raise RequestError(502, "Backend returned invalid function arguments")
            output["tool_calls"].append({"id": f"call_{nonce}_{index}", "type": "function",
                                         "function": {"name": function["name"],
                                                      "arguments": json.dumps(function["arguments"], ensure_ascii=False)}})
    finish = "length" if result.get("done_reason") == "length" else "tool_calls" if calls else "stop"
    return {"id": "chatcmpl-" + str(time.time_ns()), "object": "chat.completion",
            "created": int(time.time()), "model": LLM_MODEL,
            "choices": [{"index": 0, "message": output, "finish_reason": finish}],
            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
                      "total_tokens": prompt_tokens + completion_tokens}}


def embedding_payload(data):
    if data.get("model") != EMBED_MODEL:
        raise RequestError(400, "Only embedding model nomic-embed-text is available")
    if set(data) - {"model", "input", "truncate"}:
        raise RequestError(400, "Only model, input and truncate are supported")
    texts = data.get("input")
    texts = [texts] if isinstance(texts, str) else texts
    if (not isinstance(texts, list) or not 1 <= len(texts) <= 64
            or any(not isinstance(text, str) or not text or len(text) > 65536 for text in texts)):
        raise RequestError(400, "input must contain 1..64 nonempty texts of at most 65536 characters")
    truncate = data.get("truncate", False)
    if type(truncate) is not bool:
        raise RequestError(400, "truncate must be boolean")
    return {"model": EMBED_MODEL, "input": texts, "truncate": truncate,
            "options": {"num_ctx": 4096, "num_thread": 4}, "keep_alive": "5m"}


class Gateway(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "ask-home-engine"
    sys_version = ""

    def setup(self):
        super().setup()
        self.connection.settimeout(15)

    def log_message(self, format, *args):
        # Do not log headers, request URLs, prompts or keys.
        pass

    def reply(self, status, payload):
        body = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.send_header("Cache-Control", "no-store")
        if status == 401:
            self.send_header("WWW-Authenticate", "Bearer")
        if status == 429:
            self.send_header("Retry-After", "2")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def stream_reply(self, completion, include_usage):
        # Buffered SSE: all deltas are real backend output, not simulated tokens.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Connection", "close")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.connection.settimeout(15)
        base = {key: completion[key] for key in ("id", "created", "model")}
        base["object"] = "chat.completion.chunk"

        def event(choices, usage=None):
            chunk = dict(base, choices=choices)
            if include_usage:
                chunk["usage"] = usage
            self.wfile.write(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode())
            self.wfile.flush()

        choice = completion["choices"][0]
        message = choice["message"]
        event([{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}])
        delta = {"content": message["content"]}
        if "tool_calls" in message:
            delta["tool_calls"] = [dict(call, index=index) for index, call in enumerate(message["tool_calls"])]
        event([{"index": 0, "delta": delta, "finish_reason": None}])
        event([{"index": 0, "delta": {}, "finish_reason": choice["finish_reason"]}])
        if include_usage:
            event([], completion["usage"])
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True

    def error(self, status, message):
        if self.path == "/api/embed":
            self.reply(status, {"error": message})
        else:
            self.reply(status, {"error": {"message": message, "type": "gateway_error", "code": status}})

    def handle_request(self):
        scope = {"/v1/models": "llm", "/v1/chat/completions": "llm", "/api/embed": "embed"}.get(self.path)
        if scope is None:
            raise RequestError(404, "Unknown endpoint")
        headers = self.headers.get_all("Authorization", [])
        expected = "Bearer " + KEYS[scope]
        if len(headers) != 1 or not hmac.compare_digest(headers[0].encode(), expected.encode()):
            raise RequestError(401, "A valid endpoint-scoped Bearer key is required")
        method = "GET" if self.path == "/v1/models" else "POST"
        if self.command != method:
            raise RequestError(405, "Endpoint requires " + method)
        if self.path == "/v1/models":
            self.reply(200, {"object": "list", "data": [{"id": LLM_MODEL, "object": "model", "created": 0, "owned_by": "home-server"}]})
            return
        if self.headers.get("Transfer-Encoding"):
            raise RequestError(400, "Transfer-Encoding is not supported")
        lengths = self.headers.get_all("Content-Length", [])
        if len(lengths) != 1:
            raise RequestError(411, "Exactly one Content-Length is required")
        try:
            size = int(lengths[0])
        except ValueError:
            raise RequestError(400, "Invalid Content-Length")
        if size <= 0 or size > BODY_LIMIT:
            raise RequestError(413, "Request body must be 1..1048576 bytes")
        if self.headers.get("Content-Type", "").split(";", 1)[0].strip().lower() != "application/json":
            raise RequestError(415, "Content-Type must be application/json")
        raw = self.rfile.read(size)
        if len(raw) != size:
            raise RequestError(400, "Incomplete request body")
        try:
            data = json.loads(raw)
        except (ValueError, UnicodeError, RecursionError):
            raise RequestError(400, "Invalid JSON")
        if not isinstance(data, dict):
            raise RequestError(400, "Request JSON must be an object")
        is_chat = self.path == "/v1/chat/completions"
        payload = chat_payload(data) if is_chat else embedding_payload(data)
        if not INFERENCE.acquire(blocking=False):
            raise RequestError(429, "Another inference request is active; retry later")
        try:
            result = backend("/api/chat" if is_chat else "/api/embed", payload)
        finally:
            INFERENCE.release()
        if not is_chat:
            self.reply(200, result)
            return
        completion = chat_completion(result)
        if data.get("stream", False):
            self.stream_reply(completion, data.get("stream_options", {}).get("include_usage", False))
        else:
            self.reply(200, completion)

    def do_GET(self):
        try:
            self.handle_request()
        except RequestError as error:
            self.error(error.status, error.message)
        except TimeoutError:
            self.error(408, "Request read timed out")
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    do_POST = do_GET
    do_PUT = do_GET
    do_DELETE = do_GET
    do_HEAD = do_GET
    do_OPTIONS = do_GET
    do_PATCH = do_GET


class BoundedServer(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 8

    def __init__(self, address, handler):
        self.slots = threading.BoundedSemaphore(8)
        super().__init__(address, handler)

    def process_request(self, request, client_address):
        if not self.slots.acquire(blocking=False):
            try:
                request.settimeout(1)
                request.sendall(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nRetry-After: 2\r\nConnection: close\r\n\r\n")
            finally:
                self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.slots.release()


if __name__ == "__main__":
    server = BoundedServer(("0.0.0.0", 8080), Gateway)

    def terminate(_signal, _frame):
        # PID 1 needs an explicit handler; shutdown() here would deadlock.
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, terminate)
    try:
        server.serve_forever()
    finally:
        server.server_close()
