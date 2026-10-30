# TEST ONLY: owned loopback TLS fixture; no DNS or external connections.
import socket, ssl, sys
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[1], sys.argv[2])
sock = socket.socket(socket.AF_INET6 if ':' in sys.argv[3] else socket.AF_INET)
sock.bind((sys.argv[3], int(sys.argv[4])))
sock.listen(8)
sock.settimeout(8)
print(sock.getsockname()[1], flush=True)
try:
    while True:
        connection, _ = sock.accept()
        try:
            with ctx.wrap_socket(connection, server_side=True) as tls:
                data = tls.recv(16384)
                response = (b'HTTP/1.1 302 Found\r\nLocation: https://other.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n' if b'/redirect ' in data else b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok')
                tls.sendall(response)
                try:
                    tls.unwrap().close()
                except (ssl.SSLError, OSError):
                    pass
        except (ssl.SSLError, OSError):
            connection.close()
except TimeoutError:
    pass
