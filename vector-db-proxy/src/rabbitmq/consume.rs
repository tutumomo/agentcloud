use crate::data::chunking::{Chunking, ChunkingStrategy, TextChunker};
use crate::data::models::Document as DocumentModel;
use crate::data::models::FileType;
use crate::data::processing_incoming_messages::process_messages;
use crate::gcp::gcs::get_object_from_gcs;
use crate::qdrant::{helpers::construct_point_struct, utils::Qdrant};
use crate::rabbitmq::client::{bind_queue_to_exchange, channel_rabbitmq, connect_rabbitmq};
use crate::rabbitmq::models::RabbitConnect;

use actix_web::dev::ResourcePath;
use amqp_serde::types::ShortStr;
use amqprs::channel::{BasicAckArguments, BasicCancelArguments, BasicConsumeArguments};
use anyhow::Result;
use qdrant_client::client::QdrantClient;
use qdrant_client::prelude::PointStruct;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::{fs, fs::File};
use tokio::sync::RwLock;

async fn extract_text_from_file(
    file_type: FileType,
    file_path: &str,
) -> Option<(String, HashMap<String, String>)> {
    let mut document_text = String::new();
    let mut metadata = HashMap::new();
    let chunker = TextChunker::default();
    match file_type {
        FileType::PDF => {
            (document_text, metadata) = chunker
                .extract_text_from_pdf(file_path)
                .expect("TODO: panic message");
        }
        FileType::TXT => {
            document_text = fs::read_to_string(file_path).unwrap();
        }
        FileType::DOCX => {
            (document_text, metadata) = chunker.extract_text_from_docx(file_path).unwrap();
        }
        FileType::DOC => {}
        FileType::UNKNOWN => {}
    }
    // Once we have extracted the text from the file we no longer need the file and there file we delete from disk
    match fs::remove_file(file_path.trim_matches('"').path()) {
        Ok(_) => println!("File: {:?} successfully deleted", file_path),
        Err(e) => println!(
            "An error occurred while trying to delete file: {}. Error: {:?}",
            file_path, e
        ),
    }
    let results = (document_text, metadata);
    Some(results)
}

async fn apply_chunking_strategy_to_document(
    file_type: FileType,
    document_text: String,
    metadata: Option<HashMap<String, String>>,
    chunking_strategy: ChunkingStrategy,
) -> Result<Vec<DocumentModel>> {
    let mut chunks: Vec<DocumentModel> = vec![];
    let chunker = TextChunker::default();
    match file_type {
        FileType::PDF => {
            match chunker
                .chunk(document_text, metadata, chunking_strategy)
                .await
            {
                Ok(c) => chunks = c,
                Err(e) => println!("An error occurred: {}", e),
            }
        }
        FileType::TXT => {
            match chunker
                .chunk(document_text, metadata, chunking_strategy)
                .await
            {
                Ok(c) => chunks = c,
                Err(e) => println!("An error occurred: {}", e),
            }
        }
        FileType::DOC => {}
        FileType::DOCX => {}
        FileType::UNKNOWN => {}
    }
    Ok(chunks)
}

async fn save_file_to_disk(content: Vec<u8>, file_name: &str) -> Result<()> {
    let file_path = file_name.trim_matches('"');
    println!("File path : {}", file_path);
    let mut file = File::create(file_path)?;
    file.write_all(&*content)?; // handle errors
    Ok(())
}

pub async fn subscribe_to_queue(
    app_data: Arc<RwLock<QdrantClient>>,
    connection_details: RabbitConnect,
    exchange_name: &str,
    queue_name: &str,
    routing_key: &str,
) -> Result<()> {
    // loop {
    let mut connection = connect_rabbitmq(&connection_details).await;
    let mut channel = channel_rabbitmq(&connection).await;
    bind_queue_to_exchange(
        &mut connection,
        &mut channel,
        &connection_details,
        exchange_name,
        queue_name,
        routing_key,
    )
    .await;
    let args = BasicConsumeArguments::new(queue_name, "");
    let (ctag, mut messages_rx) = channel.basic_consume_rx(args.clone()).await.unwrap();
    while let Some(message) = messages_rx.recv().await {
        let headers = message.basic_properties.unwrap().headers().unwrap().clone();
        if let Some(stream) = headers.get(&ShortStr::try_from("stream").unwrap()) {
            let stream_string: String = stream.to_string();
            let stream_split: Vec<&str> = stream_string.split("_").collect();
            let datasource_id = stream_split.to_vec()[0];
            if let Some(msg) = message.content {
                // if the header 'type' is present then assume that it is a file upload. pull from gcs
                if let Ok(message_string) = String::from_utf8(msg.clone().to_vec()) {
                    if let Some(_) = headers.get(&ShortStr::try_from("type").unwrap()) {
                        if let Ok(_json) = serde_json::from_str(message_string.as_str()) {
                            let message_data: Value = _json; // this is necessary because  you can not do type annotation inside a if let Ok() expression
                            if let Some(bucket_name) = message_data.get("bucket") {
                                if let Some(file_name) = message_data.get("filename") {
                                    match get_object_from_gcs(
                                        bucket_name.as_str().unwrap(),
                                        file_name.as_str().unwrap(),
                                    )
                                    .await
                                    {
                                        Ok(file) => {
                                            let file_path = format!("{}", file_name);
                                            let file_path_split: Vec<&str> =
                                                file_path.split(".").collect();
                                            let file_extension =
                                                file_path_split.to_vec()[1].to_string();
                                            let file_type = FileType::from(file_extension);
                                            // The reason we are choosing to write the file to disk first is to create
                                            // parity between running locally and running in cloud
                                            save_file_to_disk(file, file_path.as_str()).await?;
                                            let (document_text, metadata) = extract_text_from_file(
                                                file_type,
                                                file_path.as_str(),
                                            )
                                            .await
                                            .unwrap();
                                            match apply_chunking_strategy_to_document(
                                                file_type,
                                                document_text,
                                                Some(metadata),
                                                ChunkingStrategy::SEMANTIC_CHUNKING,
                                            )
                                            .await
                                            {
                                                Ok(chunks) => {
                                                    let mut points_to_upload: Vec<PointStruct> =
                                                        vec![];
                                                    for element in chunks.iter() {
                                                        let mut metadata =
                                                            element.metadata.clone().unwrap();
                                                        metadata.insert(
                                                            "text".to_string(),
                                                            element.page_content.to_string(),
                                                        );
                                                        let embedding_vector =
                                                            &element.embedding_vector;
                                                        match embedding_vector {
                                                            Some(val) => {
                                                                if let Some(point_struct) =
                                                                    construct_point_struct(
                                                                        val, metadata,
                                                                    )
                                                                    .await
                                                                {
                                                                    points_to_upload
                                                                        .push(point_struct)
                                                                }
                                                            }
                                                            None => {
                                                                println!(
                                                                    "Embedding vector was empty!"
                                                                )
                                                            }
                                                        }
                                                    }
                                                    let qdrant_conn_clone = Arc::clone(&app_data);
                                                    let qdrant = Qdrant::new(
                                                        qdrant_conn_clone,
                                                        datasource_id.to_string(),
                                                    );
                                                    qdrant
                                                        .bulk_upsert_data(points_to_upload)
                                                        .await?;
                                                }
                                                Err(e) => println!("Error: {}", e),
                                            }
                                        }
                                        Err(e) => println!("Error: {}", e),
                                    }
                                }
                            }
                        }
                    } else {
                        // This is where data is coming from airbyte rather than a direct file upload
                        let qdrant_conn = Arc::clone(&app_data);
                        let args =
                            BasicAckArguments::new(message.deliver.unwrap().delivery_tag(), false);
                        let _ = channel.basic_ack(args).await;
                        let _ = process_messages(
                            qdrant_conn,
                            message_string,
                            datasource_id.to_string(),
                        )
                        .await;
                    }
                }
            }
        } else {
            println!("There was no stream_id in message... can not upload data!");
        }
    }

    // this is what to do when we get an error
    if let Err(e) = channel.basic_cancel(BasicCancelArguments::new(&ctag)).await {
        println!("error {}", e.to_string());
    };

    Ok(())
}
