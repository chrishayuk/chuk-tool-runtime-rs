"""Guest-side entrypoint for isolated code execution.

Runs INSIDE the sandbox. Stdlib-only. Reads its job (code / endpoint / token /
namespace / tool names) from the json path in argv[1], connects to the host
broker over the unix socket, executes the untrusted code with async tool proxies
bound in its globals, and reports the return value back. Matches the host
broker's length-prefixed JSON wire protocol.
"""

import asyncio
import json
import struct
import sys

_LEN = struct.Struct(">I")


async def _send(writer, obj):
    body = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    writer.write(_LEN.pack(len(body)) + body)
    await writer.drain()


async def _recv(reader):
    (length,) = _LEN.unpack(await reader.readexactly(4))
    return json.loads((await reader.readexactly(length)).decode("utf-8"))


async def main():
    with open(sys.argv[1], encoding="utf-8") as fh:
        job = json.load(fh)

    reader, writer = await asyncio.open_unix_connection(job["endpoint"])
    counter = [0]

    async def rpc(method, **fields):
        counter[0] += 1
        await _send(writer, {"id": counter[0], "method": method, **fields})
        reply = await _recv(reader)
        if not reply.get("ok"):
            raise RuntimeError(reply.get("error") or "broker rejected request")
        return reply.get("value")

    await rpc("hello", token=job["token"])

    namespace = job.get("namespace", "default")
    guest_globals = {}

    def make_proxy(name):
        async def proxy(**kwargs):
            return await rpc(
                "call_tool",
                params={"name": name, "namespace": namespace, "arguments": kwargs},
            )

        return proxy

    for name in job.get("tools", []):
        guest_globals[name] = make_proxy(name)

    # Wrap the code so it may use `await` and `return`.
    body = "".join("    " + line + "\n" for line in job["code"].splitlines()) or "    pass\n"
    exec(compile("async def __guest_main__():\n" + body, "<guest>", "exec"), guest_globals)
    result = await guest_globals["__guest_main__"]()

    await rpc("result", params={"value": result})
    writer.close()


asyncio.run(main())
