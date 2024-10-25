use frame::{OpCode, WebSocketFrame};
use http_muncher::{Parser, ParserHandler};
use mio::tcp::TcpListener;
use mio::*;
use rustc_serialize::base64::{ToBase64, STANDARD};
use std::{cell::RefCell, collections::HashMap, fmt, rc::Rc};
use tcp::TcpStream;

mod frame;

const SERVER_TOKEN: Token = Token(0);
const SERVER_ADDRESS: &str = "127.0.0.1:10000";

#[derive(Debug)]
enum ClientState {
    AwaitingHandshake(RefCell<Parser<HttpParser>>),
    HandshakeResponse,
    Connected,
}

struct WebSocketServer {
    socket: TcpListener,
    clients: HashMap<Token, WebSocketClient>,
    token_counter: usize,
}

impl Handler for WebSocketServer {
    type Timeout = usize;
    type Message = ();

    fn ready(&mut self, event_loop: &mut EventLoop<Self>, token: Token, events: EventSet) {
        if events.is_readable() {
            match token {
                SERVER_TOKEN => {
                    let client_socket = match self.socket.accept() {
                        Err(e) => {
                            println!("Accept error: {}", e);
                            return;
                        }
                        Ok(None) => unreachable!("Accept returned 'None'"),
                        Ok(Some((sock, _))) => sock,
                    };

                    self.token_counter += 1;
                    let new_token = Token(self.token_counter);

                    self.clients
                        .insert(new_token, WebSocketClient::new(client_socket));
                    event_loop
                        .register(
                            &self.clients[&new_token].socket,
                            new_token,
                            EventSet::readable(),
                            PollOpt::edge() | PollOpt::oneshot(),
                        )
                        .expect("Failed to regitster client socket")
                }
                token => {
                    let mut client = self.clients.get_mut(&token).expect("Client not found");
                    client.read();
                    event_loop
                        .reregister(
                            &client.socket,
                            token,
                            client.interest,
                            PollOpt::edge() | PollOpt::oneshot(),
                        )
                        .expect("Failed to reregister socket");
                }
            }
        }

        // Handle write events that are generated whenever
        // the socket becomes available for a write operation:
        if events.is_writable() {
            let mut client = self.clients.get_mut(&token).expect("Failed to get client");
            client.write();
            event_loop
                .reregister(
                    &client.socket,
                    token,
                    client.interest,
                    PollOpt::edge() | PollOpt::oneshot(),
                )
                .expect("Failed to reregister socket");
        }

        if events.is_hup() {
            let client = self.clients.remove(&token).expect("Client not found");

            client
                .socket
                .shutdown(std::net::Shutdown::Both)
                .expect("Socket shutdown failed");
            event_loop
                .deregister(&client.socket)
                .expect("Socket deregister failed");
        }
    }
}

impl WebSocketServer {
    fn new(socket: TcpListener) -> Self {
        Self {
            token_counter: 1,
            socket,
            clients: HashMap::new(),
        }
    }

    fn init() -> WebSocketServer {
        let address = SERVER_ADDRESS
            .parse()
            .expect("Failed to parse server address");

        let server_socket = TcpListener::bind(&address).expect("Failed on binding to socket");

        WebSocketServer::new(server_socket)
    }
}

type Headers = Rc<RefCell<HashMap<String, String>>>;

struct HttpParser {
    current_key: Option<String>,
    headers: Headers,
}

impl ParserHandler for HttpParser {
    fn on_header_field(&mut self, s: &[u8]) -> bool {
        self.current_key = Some(std::str::from_utf8(s).unwrap().to_string());
        true
    }

    fn on_header_value(&mut self, s: &[u8]) -> bool {
        self.headers.borrow_mut().insert(
            self.current_key.clone().unwrap(),
            std::str::from_utf8(s).unwrap().to_string(),
        );
        true
    }

    fn on_headers_complete(&mut self) -> bool {
        false
    }
}

struct WebSocketClient {
    socket: TcpStream,
    headers: Headers,
    interest: EventSet,
    state: ClientState,
    outgoing: Vec<WebSocketFrame>,
}

impl WebSocketClient {
    fn write_handshake(&mut self) {
        let headers = self.headers.borrow();

        let response_key = gen_key(
            &headers
                .get("Sec-WebSocket-Key")
                .expect("Failed to get Sec-WebSocket-Key header"),
        );

        let response = fmt::format(format_args!(
            "HTTP/1.1 101 Switching Protocols\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Accept: {}\r\n\
            Upgrade: websocket\r\n\r\n",
            response_key
        ));

        self.socket
            .try_write(response.as_bytes())
            .expect("Failed to write response");

        self.state = ClientState::Connected;

        self.interest.remove(EventSet::writable());
        self.interest.insert(EventSet::readable());
    }

    fn write(&mut self) {
        match &self.state {
            ClientState::HandshakeResponse => self.write_handshake(),
            ClientState::Connected => {
                let mut close_connection = false;

                println!("очередь фреймов на отправку: {}", self.outgoing.len());

                for frame in self.outgoing.iter() {
                    if let Err(e) = frame.write(&mut self.socket) {
                        println!("ошибка при записи фрейма: {}", e);
                    }

                    if frame.is_close() {
                        close_connection = true;
                    }
                }

                self.outgoing.clear();

                self.interest.remove(EventSet::writable());
                if close_connection {
                    self.interest.insert(EventSet::hup());
                } else {
                    self.interest.insert(EventSet::readable());
                }
            }
            _ => {}
        }
    }

    fn read(&mut self) {
        match &self.state {
            ClientState::AwaitingHandshake(_) => self.read_handshake(),
            ClientState::Connected => self.read_frame(),
            _ => {}
        }
    }

    fn read_frame(&mut self) {
        let frame = WebSocketFrame::read(&mut self.socket);
        match frame {
            Ok(frame) => {
                match frame.get_opcode() {
                    OpCode::TextFrame => {
                        println!("{:?}", frame);

                        let frame_reply = WebSocketFrame::from("Привет!");
                        self.outgoing.push(frame_reply);
                    }
                    OpCode::Ping => {
                        self.outgoing.push(WebSocketFrame::pong(&frame));
                    }
                    OpCode::ConnectionClose => {
                        self.outgoing.push(WebSocketFrame::close_from(&frame));
                    }
                    _ => {}
                }

                self.interest.remove(EventSet::readable());
                self.interest.insert(EventSet::writable());
            }
            Err(e) => println!("Ошибка при чтении фрейма: {}", e),
        }
    }

    fn read_handshake(&mut self) {
        loop {
            let mut buf = [0; 2048];
            match self.socket.try_read(&mut buf) {
                Err(e) => {
                    println!("Error while reading socket {:?}", e);
                    return;
                }
                Ok(None) => {
                    // Socket buffer has got no more bytes.
                    break;
                }
                Ok(Some(len)) => {
                    let is_upgrade =
                        if let ClientState::AwaitingHandshake(ref parser_state) = self.state {
                            let mut parser = parser_state.borrow_mut();
                            parser.parse(&buf[0..len]);
                            parser.is_upgrade()
                        } else {
                            false
                        };

                    if is_upgrade {
                        self.state = ClientState::HandshakeResponse;

                        self.interest.remove(EventSet::readable());
                        self.interest.insert(EventSet::writable());

                        break;
                    }
                }
            }
        }
    }

    fn new(socket: TcpStream) -> Self {
        let headers = Rc::new(RefCell::new(HashMap::new()));

        Self {
            socket,
            headers: headers.clone(),
            interest: EventSet::readable(),
            state: ClientState::AwaitingHandshake(RefCell::new(Parser::request(HttpParser {
                headers: headers.clone(),
                current_key: None,
            }))),
            outgoing: Vec::new(),
        }
    }
}

fn gen_key(key: &String) -> String {
    let mut m = sha1::Sha1::new();
    let mut buf = [0u8; 20];

    m.update(key.as_bytes());
    m.update("258EAFA5-E914-47DA-95CA-C5AB0DC85B11".as_bytes());

    m.output(&mut buf);

    buf.to_base64(STANDARD)
}

fn init_server() {
    let mut event_loop = EventLoop::new().expect("Failed to create event loop");

    let mut server = WebSocketServer::init();

    event_loop
        .register(
            &server.socket,
            SERVER_TOKEN,
            EventSet::readable(),
            PollOpt::edge(),
        )
        .expect("Failed to register server socket");

    event_loop.run(&mut server).expect("Failed to run server");
}

fn main() {
    init_server();
}
