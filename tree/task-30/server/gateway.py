#!/usr/bin/env python3
"""Single-flight inference (chat: llama-server on the iGPU, embeddings: Ollama CPU);
only the LAN gateway has a published port."""
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
CHAT_BACKEND = "http://chat:8080"
EMBED_BACKEND = "http://ollama:11434"
INFERENCE = threading.Lock()
KEYS = {"llm": os.environ["LLM_KEY"], "embed": os.environ["EMBED_KEY"]}
if any(len(key) != 64 or any(c not in "0123456789abcdef" for c in key) for key in KEYS.values()):
    raise RuntimeError("Keys must be 64 lowercase hexadecimal characters")
if hmac.compare_digest(KEYS["llm"], KEYS["embed"]):
    raise RuntimeError("Chat and embedding keys must differ")


class RequestError(Exception):
    def __init__(self, status, message):
        self.status, self.message = status, message


def open_backend(url, payload):
    """Returns the open response; HTTP and connection failures become RequestError."""
    request = urllib.request.Request(url, json.dumps(payload).encode(),
                                     {"Content-Type": "application/json"})
    try:
        return urllib.request.urlopen(request, timeout=600)
    except urllib.error.HTTPError as error:
        raw = error.read(RESPONSE_LIMIT)
        try:
            detail = json.loads(raw).get("error", "Backend rejected request")
            if isinstance(detail, dict):
                detail = detail.get("message", detail)
        except (ValueError, AttributeError):
            detail = raw.decode(errors="replace")[:4096]
        raise RequestError(error.code, str(detail)) from error
    except (TimeoutError, OSError) as error:
        raise RequestError(504 if isinstance(error, TimeoutError) else 502,
                           "Backend unavailable: " + str(error)) from error


def backend(url, payload):
    try:
        with open_backend(url, payload) as response:
            raw = response.read(RESPONSE_LIMIT + 1)
        if len(raw) > RESPONSE_LIMIT:
            raise RequestError(502, "Backend response exceeds limit")
        result = json.loads(raw)
        if not isinstance(result, dict):
            raise ValueError("Expected backend object")
        return result
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
        converted.append({"id": call_id, "type": "function",
                          "function": {"name": function["name"], "arguments": function["arguments"]}})
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
            known_calls.pop(call_id)
            item["tool_call_id"] = call_id
        elif "tool_call_id" in message:
            raise RequestError(400, "Only tool replies can carry tool_call_id")
        converted.append(item)
    return converted


def chat_payload(data):
    if data.get("model") != LLM_MODEL:
        raise RequestError(400, "Only model " + LLM_MODEL + " is available")
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
    options = {"max_tokens": 1024}
    if "max_tokens" in data and "max_completion_tokens" in data:
        raise RequestError(400, "Use only one output token limit")
    for field in ("max_tokens", "max_completion_tokens"):
        if field in data:
            value = data[field]
            if type(value) is not int or not 1 <= value <= 2048:
                raise RequestError(400, field + " must be an integer in 1..2048")
            options["max_tokens"] = value
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
        # llama.cpp disables top-k for any value <= 0.
        options["top_k"] = max(data["top_k"], 0)
    if "stop" in data:
        stop = data["stop"]
        stop = [stop] if isinstance(stop, str) else stop
        if not isinstance(stop, list) or not 1 <= len(stop) <= 4 or any(not isinstance(s, str) or not s or len(s) > 256 for s in stop):
            raise RequestError(400, "stop must contain 1..4 nonempty strings of at most 256 characters")
        options["stop"] = stop
    stream = data.get("stream", False)
    payload = dict(options, model=LLM_MODEL, messages=messages, stream=stream,
                   chat_template_kwargs={"enable_thinking": False})
    if stream:
        payload["stream_options"] = {"include_usage": True}
    if tools and choice == "auto":
        payload["tools"] = tools
    if "response_format" in data:
        fmt = data["response_format"]
        if not isinstance(fmt, dict):
            raise RequestError(400, "response_format must be an object")
        if fmt.get("type") == "json_object" and set(fmt) == {"type"}:
            payload["response_format"] = fmt
        elif fmt.get("type") == "json_schema" and isinstance(fmt.get("json_schema"), dict) and isinstance(fmt["json_schema"].get("schema"), dict):
            payload["response_format"] = {"type": "json_schema",
                                          "json_schema": {"schema": fmt["json_schema"]["schema"]}}
        elif fmt != {"type": "text"}:
            raise RequestError(400, "response_format supports text, json_object or json_schema")
    return payload


def response_calls(calls):
    """Validated OpenAI tool calls from the backend; arguments must be JSON objects."""
    if not isinstance(calls, list):
        raise RequestError(502, "Backend returned invalid tool calls")
    output = []
    for call in calls:
        function = call.get("function") if isinstance(call, dict) else None
        if (not isinstance(function, dict) or not isinstance(function.get("name"), str)
                or not isinstance(function.get("arguments"), str) or not isinstance(call.get("id"), str)):
            raise RequestError(502, "Backend returned invalid function arguments")
        try:
            if not isinstance(json.loads(function["arguments"]), dict):
                raise ValueError
        except (ValueError, RecursionError):
            raise RequestError(502, "Backend returned invalid function arguments")
        output.append({"id": call["id"], "type": "function",
                       "function": {"name": function["name"], "arguments": function["arguments"]}})
    return output


def usage(result):
    usage = result.get("usage")
    counts = [usage.get(k) if isinstance(usage, dict) else None for k in ("prompt_tokens", "completion_tokens")]
    if any(type(n) is not int or n < 0 for n in counts):
        raise RequestError(502, "Backend returned invalid token usage")
    return {"prompt_tokens": counts[0], "completion_tokens": counts[1], "total_tokens": sum(counts)}


def chat_completion(result):
    choices = result.get("choices")
    choice = choices[0] if isinstance(choices, list) and len(choices) == 1 else None
    message = choice.get("message") if isinstance(choice, dict) else None
    if not isinstance(message, dict) or not isinstance(message.get("content") or "", str):
        raise RequestError(502, "Backend returned an invalid chat message")
    calls = response_calls(message.get("tool_calls") or [])
    output = {"role": "assistant", "content": message.get("content") or ""}
    if calls:
        output["tool_calls"] = calls
    finish = choice.get("finish_reason")
    if finish not in ("stop", "length", "tool_calls"):
        raise RequestError(502, "Backend returned an invalid finish reason")
    return {"id": "chatcmpl-" + str(time.time_ns()), "object": "chat.completion",
            "created": int(time.time()), "model": LLM_MODEL,
            "choices": [{"index": 0, "message": output, "finish_reason": finish}],
            "usage": usage(result)}


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
            "options": {"num_ctx": 4096, "num_thread": 2}, "keep_alive": -1}


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

    def stream_reply(self, response, include_usage):
        """Relays backend SSE chunk by chunk; only whitelisted delta fields pass."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Connection", "close")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.close_connection = True
        base = {"id": "chatcmpl-" + str(time.time_ns()), "object": "chat.completion.chunk",
                "created": int(time.time()), "model": LLM_MODEL}

        def send(chunk):
            self.wfile.write(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode())
            self.wfile.flush()

        total = 0
        try:
            for line in response:
                total += len(line)
                if total > RESPONSE_LIMIT:
                    raise RequestError(502, "Backend response exceeds limit")
                line = line.strip()
                if not line.startswith(b"data:"):
                    continue
                data = line[5:].strip()
                if data == b"[DONE]":
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                    return
                chunk = json.loads(data)
                if not isinstance(chunk, dict) or "error" in chunk:
                    raise RequestError(502, "Backend stream error")
                choices = []
                for choice in chunk.get("choices") or []:
                    delta = choice.get("delta") or {}
                    delta = {k: delta[k] for k in ("role", "content", "tool_calls") if delta.get(k) is not None}
                    finish = choice.get("finish_reason")
                    if delta or finish:
                        choices.append({"index": 0, "delta": delta, "finish_reason": finish})
                if choices:
                    send(dict(base, choices=choices))
                elif include_usage and isinstance(chunk.get("usage"), dict):
                    send(dict(base, choices=[], usage=usage(chunk)))
            raise RequestError(502, "Backend stream ended without [DONE]")
        except (RequestError, ValueError, AttributeError, TimeoutError, OSError) as error:
            if isinstance(error, (BrokenPipeError, ConnectionResetError)):
                raise
            # Headers are already sent: report in-band, then end the stream.
            message = error.message if isinstance(error, RequestError) else "Backend stream failed"
            send({"error": {"message": message, "type": "gateway_error", "code": 502}})

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
            if not is_chat:
                result = backend(EMBED_BACKEND + "/api/embed", payload)
            elif payload["stream"]:
                # The lock is held until the last token, so a stream counts as one inference.
                with open_backend(CHAT_BACKEND + "/v1/chat/completions", payload) as response:
                    self.stream_reply(response, data.get("stream_options", {}).get("include_usage", False))
                return
            else:
                result = chat_completion(backend(CHAT_BACKEND + "/v1/chat/completions", payload))
        finally:
            INFERENCE.release()
        self.reply(200, result)

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
