use aws_sdk_sqs::types;
use tokio::task;
use std::sync::Arc;
use polars::prelude::*;
use aws_types::region::Region;
use aws_config::meta::region::RegionProviderChain;
use rand::{thread_rng, Rng};
use rand::distributions::Alphanumeric;

/// Set up the needed aws sdk Clients
/// Arc will be used because a task will spawn to process each message on queue
async fn set_up_credentials() -> (Arc<aws_sdk_sqs::Client>, Arc<aws_sdk_s3::Client>){
    
    let provider = RegionProviderChain::first_try(std::env::var("REGION").ok().map(Region::new))
                                                        .or_default_provider()
                                                        .or_else(Region::new("us-east-2"));

    //Load config from env "~/.aws/credentials"
    let config = aws_config::from_env().region(provider).load().await;
    //Create a client instance for required services
    let sqs: aws_sdk_sqs::Client = aws_sdk_sqs::Client::new(&config);
    let s3: aws_sdk_s3::Client = aws_sdk_s3::Client::new(&config);
    //Return the Arc<T> for each client
    return (Arc::new(sqs), Arc::new(s3))
}


#[::tokio::main]
async fn main(){
    //Read queue url from env vars. The program should finish if it is missing
    let queue_url : Arc<String> = Arc::new(std::env::var("QUEUE_URL")
                                 .expect("QUEUE_URL not found in ENV vars"));

    //Get clients
    let (sqs,s3) = set_up_credentials().await;
    println!("credentials got");
    //Create a empty vec to store tasks

    let mut futures: Vec<task::JoinHandle<()>> = Vec::new();
    
    //Start a looping that will stop when no messages are found
    while let Some(msg) = sqs.receive_message().queue_url(queue_url.as_str()).send().await.unwrap().messages(){

        //Clone required parameters to move while spawning tasks
        let sqs_clone: Arc<aws_sdk_sqs::Client> = Arc::clone(&sqs);
        let s3_clone: Arc<aws_sdk_s3::Client> = Arc::clone(&s3);
        let queue: Arc<String> = Arc::clone(&queue_url);
        let msg_clone = msg[0].clone();

        //Call a task::spawn to process the current message
        let task: task::JoinHandle<()> = task::spawn(async move {
            println!("Thread spawn");
            //Get receipt_handle, it is necessary to delete the message from queue when it was processed successfully
            let receipt_handle = msg_clone.receipt_handle.clone().unwrap();
            //Get required parameters to start processing
            let response = get_queue_msg(msg_clone.to_owned()).await;
           
            //Process the current message
            //When it fails the task should finish            
            let process = match response{
                Ok(x) => process_queue(x,s3_clone).await.unwrap_or(String::from("Failed")),
                Err(_) => return
                };
            
            //If process variable is Success, the message must be deleted from queue
            if process=="Success"{
                sqs_clone.delete_message().queue_url(queue.as_str()).receipt_handle(receipt_handle).send().await.unwrap()
            }else{
                return
                };

            }

        );

        futures.push(task);
    };

    //Await all tasks to finish
    for future in futures{
        future.await.expect("Task await failed")
    };

}

///Process a message from SQS and get the required informations
/// Since the goal is to process new files in S3, it should get from message the bucket_name and the object_key from the new file
/// An example of a message can be found in "event_example.txt" file
async fn get_queue_msg(msg:types::Message) -> Result<(String,String),serde_json::Error>{

    //Get msg body as String
    let msg_body = msg.body.unwrap();
    //Deserialize to Json
    let body: serde_json::Value  = serde_json::from_str(msg_body.as_str())?;
    //Get the "Message" key from body, and deserialize it
    let body_message : serde_json::Value  =  serde_json::from_str(body["Message"].as_str().unwrap())?;
    //Find the required parameters
    let s3_name:String = body_message["Records"][0]["s3"]["bucket"]["name"].to_string();
    let obj_key: String = body_message["Records"][0]["s3"]["object"]["key"].to_string();
    //Then return
    return Ok((s3_name.to_owned(),obj_key.to_owned()));
    
}


async fn process_queue(file_location:(String,String), s3: Arc<aws_sdk_s3::Client>) -> Result<String,Box<dyn std::error::Error>>{
    let (bucket,key) = file_location;

    let final_bucket: String = std::env::var("FINAL_BUCKET")
                                .expect("FINAL_BUCKET not found in ENV vars");
    //Read from S3
    let req:aws_sdk_s3::primitives::AggregatedBytes  = s3.get_object()
                                .bucket(bucket.replace('"',""))
                                .key(key.replace('"',""))
                                .send()
                                .await?
                                .body
                                .collect()
                                .await?;

    //Data prep
    let mut lf: LazyFrame = CsvReader::new(std::io::Cursor::new(req.into_bytes()))
                                    .has_header(true)
                                    .with_delimiter(b';')
                                    .infer_schema(Some(50))
                                    .with_columns(Some(vec![String::from("sintomas"),
                                                            String::from("profissionalSaude"),
                                                            String::from("racaCor"),
                                                            String::from("sexo"),
                                                            String::from("municipio"),
                                                            String::from("idade")]))
                                    .finish().unwrap().lazy();
    
    lf = lf.filter(col("sexo").eq(lit("Masculino"))
                                    .or(col("sexo").eq(lit("Feminino")))                          
                    )
            .filter(col("municipio").eq(lit("Rio de Janeiro"))
                    .or(col("municipio").eq(lit("São João de Meriti")))
                    .or(col("municipio").eq(lit("Niterói")))
                    .or(col("municipio").eq(lit("Petrópolis")))
                    .or(col("municipio").eq(lit("Cabo Frio")))
                    .or(col("municipio").eq(lit("Angra dos Reis")))
                    .or(col("municipio").eq(lit("Duque de Caxias")))
                );


    //Data partitions
    let lf: Vec<DataFrame> = lf.collect().unwrap().partition_by(&["sexo","municipio","profissionalSaude"], true)?;

    //Loop on all chunks made by 'partition_by' operation
    for mut chunk in lf{
        let mut cursor: std::io::Cursor<Vec<u8>> = std::io::Cursor::new(Vec::new());

        ParquetWriter::new(&mut cursor).with_compression(ParquetCompression::Snappy).finish(&mut chunk)?;

        let municipio = &chunk.column("municipio")?.get(0)?;

        let saude = &chunk.column("profissionalSaude")?.get(0)?;
        
        let sexo = &chunk.column("sexo")?.get(0)?;

        let body:aws_sdk_s3::primitives::ByteStream = aws_sdk_s3::primitives::ByteStream::from(cursor.into_inner());

        let rand_string: String = thread_rng()
                                    .sample_iter(&Alphanumeric)
                                    .take(30)
                                    .map(char::from)
                                    .collect();

        let filename: String = format!("municipio={0}/profissionalSaude={1}/sexo={2}/{3}.parquet",municipio,saude,sexo,rand_string);

        let _response:aws_sdk_s3::operation::put_object::PutObjectOutput = s3
                            .put_object()
                            .bucket(&final_bucket)
                            .key(filename)
                            .body(body)
                            .send()
                            .await?;
    };

    Ok(String::from("Success"))
    
}