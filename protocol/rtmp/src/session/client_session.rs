use {
    super::{
        define,
        errors::{SessionError, SessionErrorValue},
    },
    crate::{
        amf0::Amf0ValueType,
        chunk::{
            define::{chunk_type, csid_type, CHUNK_SIZE},
            packetizer::ChunkPacketizer,
            unpacketizer::{ChunkUnpacketizer, UnpackResult},
            ChunkInfo,
        },
        handshake::handshake::{ClientHandshakeState, SimpleHandshakeClient},
        messages::{
            define::{msg_type_id, RtmpMessageData},
            parser::MessageParser,
        },
        netconnection::commands::{ConnectProperties, NetConnection},
        netstream::writer::NetStreamWriter,
        protocol_control_messages::writer::ProtocolControlMessagesWriter,
        user_control_messages::writer::EventMessagesWriter,
    },
    bytes::BytesMut,
    netio::{
        bytes_writer::{AsyncBytesWriter, BytesWriter},
        netio::NetworkIO,
    },
    std::{collections::HashMap, sync::Arc},
    tokio::{net::TcpStream, sync::Mutex},
};

#[allow(dead_code)]
enum ClientSessionState {
    Handshake,
    Connect,
    CreateStream,
    Play,
    PublishingContent,
}

#[allow(dead_code)]
enum ClientSessionPlayState {
    Handshake,
    Connect,
    CreateStream,
    Play,
}

#[allow(dead_code)]
enum ClientSessionPublishState {
    Handshake,
    Connect,
    CreateStream,
    PublishingContent,
}
#[allow(dead_code)]
enum ClientType {
    Play,
    Publish,
}
pub struct ClientSession {
    packetizer: ChunkPacketizer,
    unpacketizer: ChunkUnpacketizer,
    handshaker: SimpleHandshakeClient,
    io: Arc<Mutex<NetworkIO>>,

    state: ClientSessionState,
    client_type: ClientType,
    stream_name: String,
    session_type: u8,
}

impl ClientSession {
    #[allow(dead_code)]
    fn new(stream: TcpStream, client_type: ClientType, stream_name: String) -> Self {
        let net_io = Arc::new(Mutex::new(NetworkIO::new(stream)));

        // let reader = BytesReader::new(BytesMut::new());

        Self {
            io: Arc::clone(&net_io),

            packetizer: ChunkPacketizer::new(Arc::clone(&net_io)),
            unpacketizer: ChunkUnpacketizer::new(),
            handshaker: SimpleHandshakeClient::new(Arc::clone(&net_io)),

            state: ClientSessionState::Handshake,
            client_type: client_type,
            stream_name: stream_name,
            session_type: 0,
        }
    }

    pub async fn run(&mut self) -> Result<(), SessionError> {
        loop {
            match self.state {
                ClientSessionState::Handshake => {
                    self.handshake().await?;
                }
                ClientSessionState::Connect => {
                    self.send_connect(&(define::TRANSACTION_ID_CONNECT as f64))
                        .await?;
                }
                ClientSessionState::CreateStream => {
                    self.send_create_stream(&(define::TRANSACTION_ID_CREATE_STREAM as f64))
                        .await?;
                }
                ClientSessionState::Play => {
                    self.send_play(&0.0, &self.stream_name.clone(), &0.0, &0.0, &false)
                        .await?;
                }
                ClientSessionState::PublishingContent => {
                    self.send_publish(&0.0, &self.stream_name.clone(), &"live".to_string())
                        .await?;
                }
            }

            let data = self.io.lock().await.read().await?;
            self.unpacketizer.extend_data(&data[..]);
            let result = self.unpacketizer.read_chunk()?;

            match result {
                UnpackResult::ChunkInfo(chunk_info) => {
                    let mut message_parser = MessageParser::new(chunk_info, self.session_type);
                    let mut msg = message_parser.parse()?;

                    self.process_messages(&mut msg).await?;
                }
                _ => {}
            }
        }

        // Ok(())
    }

    async fn handshake(&mut self) -> Result<(), SessionError> {
        loop {
            self.handshaker.handshake().await?;
            if self.handshaker.state == ClientHandshakeState::Finish {
                break;
            }

            let data = self.io.lock().await.read().await?;
            self.handshaker.extend_data(&data[..]);
        }
        self.state = ClientSessionState::Connect;

        Ok(())
    }

    pub async fn process_messages(
        &mut self,
        msg: &mut RtmpMessageData,
    ) -> Result<(), SessionError> {
        match msg {
            RtmpMessageData::Amf0Command {
                command_name,
                transaction_id,
                command_object,
                others,
            } => self.process_amf0_command_message(
                command_name,
                transaction_id,
                command_object,
                others,
            )?,
            RtmpMessageData::SetPeerBandwidth { properties } => {
                print!("{}", properties.window_size);
                self.on_set_peer_bandwidth().await?
            }
            RtmpMessageData::SetChunkSize { chunk_size } => self.on_set_chunk_size(chunk_size)?,
            RtmpMessageData::AudioData { data } => {
                let _ = data.len();
            }
            RtmpMessageData::VideoData { data } => {
                let _ = data.len();
            }

            _ => {}
        }
        Ok(())
    }

    pub fn process_amf0_command_message(
        &mut self,
        command_name: &Amf0ValueType,
        transaction_id: &Amf0ValueType,
        command_object: &Amf0ValueType,
        others: &mut Vec<Amf0ValueType>,
    ) -> Result<(), SessionError> {
        let empty_cmd_name = &String::new();
        let cmd_name = match command_name {
            Amf0ValueType::UTF8String(str) => str,
            _ => empty_cmd_name,
        };

        let transaction_id = match transaction_id {
            Amf0ValueType::Number(number) => number.clone() as u8,
            _ => 0,
        };

        let empty_cmd_obj: HashMap<String, Amf0ValueType> = HashMap::new();
        let _ = match command_object {
            Amf0ValueType::Object(obj) => obj,
            // Amf0ValueType::Null =>
            _ => &empty_cmd_obj,
        };

        match cmd_name.as_str() {
            "_reslut" => match transaction_id {
                define::TRANSACTION_ID_CONNECT => {
                    self.on_result_connect()?;
                }
                define::TRANSACTION_ID_CREATE_STREAM => {
                    self.on_result_create_stream()?;
                }
                _ => {}
            },
            "_error" => {
                self.on_error()?;
            }
            "onStatus" => {
                match others.remove(0) {
                    Amf0ValueType::Object(obj) => self.on_status(&obj)?,
                    _ => {
                        return Err(SessionError {
                            value: SessionErrorValue::Amf0ValueCountNotCorrect,
                        })
                    }
                };
            }

            _ => {}
        }

        Ok(())
    }

    pub async fn send_connect(&mut self, transaction_id: &f64) -> Result<(), SessionError> {
        let app_name = String::from("app");
        let properties = ConnectProperties::new(app_name);

        let mut netconnection = NetConnection::new(BytesWriter::new());
        let data = netconnection.connect(transaction_id, &properties)?;

        let mut chunk_info = ChunkInfo::new(
            csid_type::COMMAND_AMF0_AMF3,
            chunk_type::TYPE_0,
            0,
            data.len() as u32,
            msg_type_id::COMMAND_AMF0,
            0,
            data,
        );

        self.packetizer.write_chunk(&mut chunk_info).await?;
        Ok(())
    }

    pub async fn send_create_stream(&mut self, transaction_id: &f64) -> Result<(), SessionError> {
        let mut netconnection = NetConnection::new(BytesWriter::new());
        let data = netconnection.create_stream(transaction_id)?;

        let mut chunk_info = ChunkInfo::new(
            csid_type::COMMAND_AMF0_AMF3,
            chunk_type::TYPE_0,
            0,
            data.len() as u32,
            msg_type_id::COMMAND_AMF0,
            0,
            data,
        );

        self.packetizer.write_chunk(&mut chunk_info).await?;

        Ok(())
    }

    pub async fn send_delete_stream(
        &mut self,
        transaction_id: &f64,
        stream_id: &f64,
    ) -> Result<(), SessionError> {
        let mut netstream = NetStreamWriter::new(BytesWriter::new(), Arc::clone(&self.io));
        netstream.delete_stream(transaction_id, stream_id).await?;

        Ok(())
    }

    pub async fn send_publish(
        &mut self,
        transaction_id: &f64,
        stream_name: &String,
        stream_type: &String,
    ) -> Result<(), SessionError> {
        let mut netstream = NetStreamWriter::new(BytesWriter::new(), Arc::clone(&self.io));
        netstream
            .publish(transaction_id, stream_name, stream_type)
            .await?;

        Ok(())
    }

    pub async fn send_play(
        &mut self,
        transaction_id: &f64,
        stream_name: &String,
        start: &f64,
        duration: &f64,
        reset: &bool,
    ) -> Result<(), SessionError> {
        let mut netstream = NetStreamWriter::new(BytesWriter::new(), Arc::clone(&self.io));
        netstream
            .play(transaction_id, stream_name, start, duration, reset)
            .await?;

        Ok(())
    }

    pub async fn send_set_chunk_size(&mut self) -> Result<(), SessionError> {
        let mut controlmessage =
            ProtocolControlMessagesWriter::new(AsyncBytesWriter::new(self.io.clone()));
        controlmessage.write_set_chunk_size(CHUNK_SIZE).await?;
        Ok(())
    }

    pub async fn send_window_acknowledgement_size(
        &mut self,
        window_size: u32,
    ) -> Result<(), SessionError> {
        let mut controlmessage =
            ProtocolControlMessagesWriter::new(AsyncBytesWriter::new(self.io.clone()));
        controlmessage
            .write_window_acknowledgement_size(window_size)
            .await?;
        Ok(())
    }

    pub async fn send_set_buffer_length(
        &mut self,
        stream_id: u32,
        ms: u32,
    ) -> Result<(), SessionError> {
        let mut eventmessages = EventMessagesWriter::new(AsyncBytesWriter::new(self.io.clone()));
        eventmessages.write_set_buffer_length(stream_id, ms).await?;

        Ok(())
    }

    pub async fn send_audio(&mut self, data: BytesMut) -> Result<(), SessionError> {
        let mut chunk_info = ChunkInfo::new(
            csid_type::AUDIO,
            chunk_type::TYPE_0,
            0,
            data.len() as u32,
            msg_type_id::AUDIO,
            0,
            data,
        );

        self.packetizer.write_chunk(&mut chunk_info).await?;

        Ok(())
    }

    pub async fn send_video(&mut self, data: BytesMut) -> Result<(), SessionError> {
        let mut chunk_info = ChunkInfo::new(
            csid_type::VIDEO,
            chunk_type::TYPE_0,
            0,
            data.len() as u32,
            msg_type_id::VIDEO,
            0,
            data,
        );

        self.packetizer.write_chunk(&mut chunk_info).await?;

        Ok(())
    }

    pub fn on_result_connect(&mut self) -> Result<(), SessionError> {
        self.state = ClientSessionState::CreateStream;
        Ok(())
    }

    pub fn on_result_create_stream(&mut self) -> Result<(), SessionError> {
        match self.client_type {
            ClientType::Play => {
                self.state = ClientSessionState::Play;
            }
            ClientType::Publish => {
                self.state = ClientSessionState::PublishingContent;
            }
        }
        Ok(())
    }

    pub fn on_set_chunk_size(&mut self, chunk_size: &mut u32) -> Result<(), SessionError> {
        self.unpacketizer
            .update_max_chunk_size(chunk_size.clone() as usize);
        Ok(())
    }

    pub async fn on_set_peer_bandwidth(&mut self) -> Result<(), SessionError> {
        self.send_window_acknowledgement_size(250000).await?;
        Ok(())
    }
    pub fn on_error(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    pub fn on_status(&mut self, obj: &HashMap<String, Amf0ValueType>) -> Result<(), SessionError> {
        println!("{}", obj.len());
        Ok(())
    }
}
