use super::flight3::*;
use super::*;
use crate::content::*;
use crate::crypto::*;
use crate::curve::named_curve::*;
use crate::curve::*;
use crate::errors::*;
use crate::handshake::handshake_message_certificate::*;
use crate::handshake::handshake_message_certificate_verify::*;
use crate::handshake::handshake_message_client_key_exchange::*;
use crate::handshake::handshake_message_finished::*;
use crate::handshake::handshake_message_server_key_exchange::*;
use crate::handshake::*;
use crate::prf::*;
use crate::record_layer::record_layer_header::*;
use crate::record_layer::*;
use crate::signature_hash_algorithm::*;

use util::Error;

use crate::change_cipher_spec::ChangeCipherSpec;
use std::io::{BufReader, BufWriter};

use async_trait::async_trait;

pub(crate) struct Flight5;

#[async_trait]
impl Flight for Flight5 {
    fn to_string(&self) -> String {
        "Flight5".to_owned()
    }

    fn is_last_recv_flight(&self) -> bool {
        true
    }

    async fn parse(
        &self,
        _tx: &mut mpsc::Sender<()>,
        state: &mut State,
        cache: &HandshakeCache,
        cfg: &HandshakeConfig,
    ) -> Result<Box<dyn Flight + Send + Sync>, (Option<Alert>, Option<Error>)> {
        let (_seq, msgs) = match cache
            .full_pull_map(
                0,
                &[HandshakeCachePullRule {
                    typ: HandshakeType::Finished,
                    epoch: cfg.initial_epoch + 1,
                    is_client: false,
                    optional: false,
                }],
            )
            .await
        {
            Ok((seq, msgs)) => (seq, msgs),
            Err(_) => return Err((None, None)),
        };

        let finished = if let Some(message) = msgs.get(&HandshakeType::Finished) {
            match message {
                HandshakeMessage::Finished(h) => h,
                _ => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        None,
                    ))
                }
            }
        } else {
            return Err((
                Some(Alert {
                    alert_level: AlertLevel::Fatal,
                    alert_description: AlertDescription::InternalError,
                }),
                None,
            ));
        };

        let plain_text = cache
            .pull_and_merge(&[
                HandshakeCachePullRule {
                    typ: HandshakeType::ClientHello,
                    epoch: cfg.initial_epoch,
                    is_client: true,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::ServerHello,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::Certificate,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::ServerKeyExchange,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::CertificateRequest,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::ServerHelloDone,
                    epoch: cfg.initial_epoch,
                    is_client: false,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::Certificate,
                    epoch: cfg.initial_epoch,
                    is_client: true,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::ClientKeyExchange,
                    epoch: cfg.initial_epoch,
                    is_client: true,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::CertificateVerify,
                    epoch: cfg.initial_epoch,
                    is_client: true,
                    optional: false,
                },
                HandshakeCachePullRule {
                    typ: HandshakeType::Finished,
                    epoch: cfg.initial_epoch + 1,
                    is_client: true,
                    optional: false,
                },
            ])
            .await;

        if let Some(cipher_suite) = &*state.cipher_suite {
            let expected_verify_data = match prf_verify_data_server(
                &state.master_secret,
                &plain_text,
                cipher_suite.hash_func(),
            ) {
                Ok(d) => d,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InsufficientSecurity,
                        }),
                        Some(err),
                    ))
                }
            };

            if expected_verify_data != finished.verify_data {
                return Err((
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::HandshakeFailure,
                    }),
                    Some(ERR_VERIFY_DATA_MISMATCH.clone()),
                ));
            }
        }

        Ok(Box::new(Flight5 {}))
    }

    async fn generate(
        &self,
        state: &mut State,
        cache: &HandshakeCache,
        cfg: &HandshakeConfig,
    ) -> Result<Vec<Packet>, (Option<Alert>, Option<Error>)> {
        let certificate = if !cfg.local_certificates.is_empty() {
            let cert = match cfg.get_certificate(&cfg.server_name) {
                Ok(cert) => cert,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::HandshakeFailure,
                        }),
                        Some(err),
                    ))
                }
            };
            Some(cert)
        } else {
            None
        };

        let mut pkts = vec![];

        if state.remote_requested_certificate {
            pkts.push(Packet {
                record: RecordLayer::new(
                    PROTOCOL_VERSION1_2,
                    0,
                    Content::Handshake(Handshake::new(HandshakeMessage::Certificate(
                        HandshakeMessageCertificate {
                            certificate: vec![if let Some(cert) = &certificate {
                                cert.certificate.clone()
                            } else {
                                vec![]
                            }],
                        },
                    ))),
                ),
                should_encrypt: false,
                reset_local_sequence_number: false,
            });
        }

        let mut client_key_exchange = HandshakeMessageClientKeyExchange {
            identity_hint: vec![],
            public_key: vec![],
        };
        if cfg.local_psk_callback.is_none() {
            if let Some(local_keypair) = &state.local_keypair {
                client_key_exchange.public_key = local_keypair.public_key.clone();
            }
        } else {
            client_key_exchange.identity_hint = cfg.local_psk_identity_hint.clone();
        }

        pkts.push(Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                0,
                Content::Handshake(Handshake::new(HandshakeMessage::ClientKeyExchange(
                    client_key_exchange,
                ))),
            ),
            should_encrypt: false,
            reset_local_sequence_number: false,
        });

        let server_key_exchange_data = cache
            .pull_and_merge(&[HandshakeCachePullRule {
                typ: HandshakeType::ServerKeyExchange,
                epoch: cfg.initial_epoch,
                is_client: false,
                optional: false,
            }])
            .await;

        let mut server_key_exchange = HandshakeMessageServerKeyExchange {
            identity_hint: vec![],
            elliptic_curve_type: EllipticCurveType::Unsupported,
            named_curve: NamedCurve::Unsupported,
            public_key: vec![],
            hash_algorithm: HashAlgorithm::Unsupported,
            signature_algorithm: SignatureAlgorithm::Unsupported,
            signature: vec![],
        };

        // handshakeMessageServerKeyExchange is optional for PSK
        if server_key_exchange_data.is_empty() {
            if let Err((alert, err)) = handle_server_key_exchange(state, cfg, &server_key_exchange)
            {
                return Err((alert, err));
            }
        } else {
            let mut reader = BufReader::new(server_key_exchange_data.as_slice());
            let raw_handshake = match Handshake::unmarshal(&mut reader) {
                Ok(h) => h,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::UnexpectedMessage,
                        }),
                        Some(err),
                    ))
                }
            };

            match raw_handshake.handshake_message {
                HandshakeMessage::ServerKeyExchange(h) => server_key_exchange = h,
                _ => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::UnexpectedMessage,
                        }),
                        Some(ERR_INVALID_CONTENT_TYPE.clone()),
                    ))
                }
            };
        }

        // Append not-yet-sent packets
        let mut merged = vec![];
        let mut seq_pred = state.handshake_send_sequence as u16;
        for p in &mut pkts {
            let h = match &mut p.record.content {
                Content::Handshake(h) => h,
                _ => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(ERR_INVALID_CONTENT_TYPE.clone()),
                    ))
                }
            };
            h.handshake_header.message_sequence = seq_pred;
            seq_pred += 1;

            let mut raw = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(raw.as_mut());
                if let Err(err) = h.marshal(&mut writer) {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(err),
                    ));
                }
            }

            merged.extend_from_slice(&raw);
        }

        if let Err((alert, err)) =
            initalize_cipher_suite(state, cache, cfg, &server_key_exchange, &merged).await
        {
            return Err((alert, err));
        }

        // If the client has sent a certificate with signing ability, a digitally-signed
        // CertificateVerify message is sent to explicitly verify possession of the
        // private key in the certificate.
        if state.remote_requested_certificate && !cfg.local_certificates.is_empty() {
            let mut plain_text = cache
                .pull_and_merge(&[
                    HandshakeCachePullRule {
                        typ: HandshakeType::ClientHello,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ServerHello,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::Certificate,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ServerKeyExchange,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::CertificateRequest,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ServerHelloDone,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::Certificate,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ClientKeyExchange,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                ])
                .await;

            plain_text.extend_from_slice(&merged);

            // Find compatible signature scheme
            let signature_hash_algo = match select_signature_scheme(
                &cfg.local_signature_schemes,
                &certificate.as_ref().unwrap().private_key,
            ) {
                Ok(s) => s,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InsufficientSecurity,
                        }),
                        Some(err),
                    ))
                }
            };

            let cert_verify = match generate_certificate_verify(
                &plain_text,
                &certificate.as_ref().unwrap().private_key, /*, signature_hash_algo.hash*/
            ) {
                Ok(cert) => cert,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(err),
                    ))
                }
            };
            state.local_certificates_verify = cert_verify;

            let mut p = Packet {
                record: RecordLayer::new(
                    PROTOCOL_VERSION1_2,
                    0,
                    Content::Handshake(Handshake::new(HandshakeMessage::CertificateVerify(
                        HandshakeMessageCertificateVerify {
                            hash_algorithm: signature_hash_algo.hash,
                            signature_algorithm: signature_hash_algo.signature,
                            signature: state.local_certificates_verify.clone(),
                        },
                    ))),
                ),
                should_encrypt: false,
                reset_local_sequence_number: false,
            };

            let h = match &mut p.record.content {
                Content::Handshake(h) => h,
                _ => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(ERR_INVALID_CONTENT_TYPE.clone()),
                    ))
                }
            };
            h.handshake_header.message_sequence = seq_pred;

            // seqPred++ // this is the last use of seqPred

            let mut raw = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(raw.as_mut());
                if let Err(err) = h.marshal(&mut writer) {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(err),
                    ));
                }
            }
            merged.extend_from_slice(&raw);

            pkts.push(p);
        }

        pkts.push(Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                0,
                Content::ChangeCipherSpec(ChangeCipherSpec {}),
            ),
            should_encrypt: false,
            reset_local_sequence_number: false,
        });

        if state.local_verify_data.is_empty() {
            let mut plain_text = cache
                .pull_and_merge(&[
                    HandshakeCachePullRule {
                        typ: HandshakeType::ClientHello,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ServerHello,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::Certificate,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ServerKeyExchange,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::CertificateRequest,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ServerHelloDone,
                        epoch: cfg.initial_epoch,
                        is_client: false,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::Certificate,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::ClientKeyExchange,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::CertificateVerify,
                        epoch: cfg.initial_epoch,
                        is_client: true,
                        optional: false,
                    },
                    HandshakeCachePullRule {
                        typ: HandshakeType::Finished,
                        epoch: cfg.initial_epoch + 1,
                        is_client: true,
                        optional: false,
                    },
                ])
                .await;

            plain_text.extend_from_slice(&merged);
            if let Some(cipher_suite) = &*state.cipher_suite {
                state.local_verify_data = match prf_verify_data_client(
                    &state.master_secret,
                    &plain_text,
                    cipher_suite.hash_func(),
                ) {
                    Ok(data) => data,
                    Err(err) => {
                        return Err((
                            Some(Alert {
                                alert_level: AlertLevel::Fatal,
                                alert_description: AlertDescription::InternalError,
                            }),
                            Some(err),
                        ))
                    }
                };
            }
        }

        pkts.push(Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                1,
                Content::Handshake(Handshake::new(HandshakeMessage::Finished(
                    HandshakeMessageFinished {
                        verify_data: state.local_verify_data.clone(),
                    },
                ))),
            ),
            should_encrypt: true,
            reset_local_sequence_number: true,
        });

        Ok(pkts)
    }
}
async fn initalize_cipher_suite(
    state: &mut State,
    cache: &HandshakeCache,
    cfg: &HandshakeConfig,
    h: &HandshakeMessageServerKeyExchange,
    sending_plain_text: &[u8],
) -> Result<(), (Option<Alert>, Option<Error>)> {
    if let Some(cipher_suite) = &*state.cipher_suite {
        if cipher_suite.is_initialized().await {
            return Ok(());
        }
    }

    let mut client_random = vec![];
    {
        let mut writer = BufWriter::<&mut Vec<u8>>::new(client_random.as_mut());
        let _ = state.local_random.marshal(&mut writer);
    }
    let mut server_random = vec![];
    {
        let mut writer = BufWriter::<&mut Vec<u8>>::new(server_random.as_mut());
        let _ = state.remote_random.marshal(&mut writer);
    }

    if let Some(cipher_suite) = &*state.cipher_suite {
        if state.extended_master_secret {
            let session_hash = match cache
                .session_hash(
                    cipher_suite.hash_func(),
                    cfg.initial_epoch,
                    &sending_plain_text,
                )
                .await
            {
                Ok(s) => s,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(err),
                    ))
                }
            };

            state.master_secret = match prf_extended_master_secret(
                &state.pre_master_secret,
                &session_hash,
                cipher_suite.hash_func(),
            ) {
                Ok(m) => m,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::IllegalParameter,
                        }),
                        Some(err),
                    ))
                }
            };
        } else {
            state.master_secret = match prf_master_secret(
                &state.pre_master_secret,
                &client_random,
                &server_random,
                cipher_suite.hash_func(),
            ) {
                Ok(m) => m,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::InternalError,
                        }),
                        Some(err),
                    ))
                }
            };
        }
    }

    if cfg.local_psk_callback.is_none() {
        // Verify that the pair of hash algorithm and signiture is listed.
        let mut valid_signature_scheme = false;
        for ss in &cfg.local_signature_schemes {
            if ss.hash == h.hash_algorithm && ss.signature == h.signature_algorithm {
                valid_signature_scheme = true;
                break;
            }
        }
        if !valid_signature_scheme {
            return Err((
                Some(Alert {
                    alert_level: AlertLevel::Fatal,
                    alert_description: AlertDescription::InsufficientSecurity,
                }),
                Some(ERR_NO_AVAILABLE_SIGNATURE_SCHEMES.clone()),
            ));
        }

        let expected_msg =
            value_key_message(&client_random, &server_random, &h.public_key, h.named_curve);
        if let Err(err) = verify_key_signature(
            &expected_msg,
            &h.signature,
            /*h.hash_algorithm,*/ &state.peer_certificates[0],
        ) {
            return Err((
                Some(Alert {
                    alert_level: AlertLevel::Fatal,
                    alert_description: AlertDescription::BadCertificate,
                }),
                Some(err),
            ));
        }

        let mut chains = vec![];
        if !cfg.insecure_skip_verify {
            chains = match verify_cert(
                &state.peer_certificates[0], /*cfg.rootCAs, cfg.serverName*/
            ) {
                Ok(chains) => chains,
                Err(err) => {
                    return Err((
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::BadCertificate,
                        }),
                        Some(err),
                    ))
                }
            }
        }
        if let Some(verify_peer_certificate) = &cfg.verify_peer_certificate {
            if let Err(err) = verify_peer_certificate(&state.peer_certificates[0], &chains) {
                return Err((
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::BadCertificate,
                    }),
                    Some(err),
                ));
            }
        }
    }

    if let Some(cipher_suite) = &*state.cipher_suite {
        if let Err(err) = cipher_suite
            .init(&state.master_secret, &client_random, &server_random, true)
            .await
        {
            return Err((
                Some(Alert {
                    alert_level: AlertLevel::Fatal,
                    alert_description: AlertDescription::InternalError,
                }),
                Some(err),
            ));
        }
    }

    Ok(())
}