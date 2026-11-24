// Send through `notts serve` over its unix socket. Usage: node serve_client.mjs SOCKET APP TITLE BODY
import http from "node:http";

const [sock, app, title, body] = process.argv.slice(2);
const payload = JSON.stringify({ app, severity: "warn", title, body });
const req = http.request(
  { socketPath: sock, path: "/v1/send", method: "POST", headers: { "content-type": "application/json", "content-length": Buffer.byteLength(payload) } },
  (res) => {
    let data = "";
    res.setEncoding("utf8");
    res.on("data", (c) => (data += c));
    // 200 delivered, 202 queued in the outbox, 429 slow down (see Retry-After), 502 not delivered
    res.on("end", () => console.log(res.statusCode, data));
  },
);
req.on("error", (e) => {
  console.error(e.message);
  process.exit(1);
});
req.end(payload);
