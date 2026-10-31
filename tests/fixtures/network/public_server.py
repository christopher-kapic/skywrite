# TEST ONLY. Owned TLS boundary for bounded anonymous public diagnostics.
import socket, ssl, sys
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[1], sys.argv[2])
sock = socket.socket()
sock.bind(('127.0.0.1', 0))
sock.listen(8)
sock.settimeout(12)
print(sock.getsockname()[1], flush=True)
try:
    while True:
        connection, _ = sock.accept()
        try:
            with ctx.wrap_socket(connection, server_side=True) as tls:
                data = b''
                while b'\r\n\r\n' not in data and len(data) < 16384:
                    more = tls.recv(16384)
                    if not more: break
                    data += more
                path = data.split(b' ')[1]
                lower = data.lower()
                valid = (b'\r\nrange: bytes=0-1023\r\n' in lower
                    and b'\r\nuser-agent: skywrite/' in lower
                    and b'\r\naccept-encoding: identity\r\n' in lower
                    and b'authorization:' not in lower and b'cookie:' not in lower
                    and b'x-amz-' not in lower and b'x-api-key:' not in lower)
                status, extra, body = b'206 Partial Content', b'Content-Range: bytes 0-1023/8192\r\n', b'x' * 1024
                if not valid:
                    status, extra, body = b'401 Unauthorized', b'', b'forbidden request headers'
                elif path == b'/denied':
                    status, extra, body = b'403 Forbidden', b'', b'private bucket'
                elif path == b'/small':
                    status, extra, body = b'200 OK', b'', b'ok'
                elif path == b'/oversized':
                    body = b'x' * 8192
                elif path == b'/ignored-range':
                    status, extra, body = b'200 OK', b'', b'x' * 8192
                elif path == b'/short':
                    body = b'x' * 10
                elif path == b'/wrong-range':
                    extra = b'Content-Range: bytes 1-1024/8192\r\n'
                elif path in (b'/redirect', b'/private', b'/http', b'/userinfo', b'/loop'):
                    target = {b'/redirect': b'/success', b'/private': b'https://127.0.0.1/',
                        b'/http': b'http://pinned.invalid/', b'/userinfo': b'https://user:secret@pinned.invalid/',
                        b'/loop': b'/loop'}[path]
                    status, extra, body = b'302 Found', b'Location: ' + target + b'\r\nSet-Cookie: secret=never-forward\r\n', b''
                tls.sendall(b'HTTP/1.1 ' + status + b'\r\n' + extra
                    + b'Content-Type: video/mp4\r\nContent-Length: ' + str(len(body)).encode()
                    + b'\r\nConnection: close\r\n\r\n' + body)
                try:
                    tls.unwrap().close()
                except (ssl.SSLError, OSError): pass
        except (ssl.SSLError, OSError):
            connection.close()
except TimeoutError:
    pass
