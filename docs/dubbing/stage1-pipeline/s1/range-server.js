// Serve one local file over HTTP with Range support and CORS *, on a port.
//   node range-server.js <file> <port>
const http = require("http");
const fs = require("fs");
const path = require("path");
const [file, port] = [process.argv[2], Number(process.argv[3] || 8766)];
const size = fs.statSync(file).size;
const name = path.basename(file);
http.createServer((req, res) => {
  const headers = { "Accept-Ranges": "bytes", "Access-Control-Allow-Origin": "*", "Content-Type": "video/x-matroska" };
  const m = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range || "");
  if (req.method === "HEAD") { res.writeHead(200, { ...headers, "Content-Length": size }); return res.end(); }
  if (m) {
    const start = m[1] ? Number(m[1]) : Math.max(0, size - Number(m[2]));
    const end = m[1] && m[2] ? Math.min(Number(m[2]), size - 1) : size - 1;
    res.writeHead(206, { ...headers, "Content-Range": `bytes ${start}-${end}/${size}`, "Content-Length": end - start + 1 });
    return fs.createReadStream(file, { start, end }).pipe(res);
  }
  res.writeHead(200, { ...headers, "Content-Length": size });
  fs.createReadStream(file).pipe(res);
}).listen(port, "127.0.0.1", () => console.log(`serving ${name} (${size} bytes) on http://127.0.0.1:${port}/${encodeURIComponent(name)}`));
