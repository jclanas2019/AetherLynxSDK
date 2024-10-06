use reqwest::Client;
use serde_json::Value;

const API_BASE_URL: &str = "https://api.university.edu";

pub async fn get_student_info(student_id: &str) -> Result<Value, reqwest::Error> {
    let client = Client::new();
    let url = format!("{}/students/{}", API_BASE_URL, student_id);
    let response = client.get(&url).send().await?;
    let data = response.json::<Value>().await?;
    Ok(data)
}

pub async fn get_financial_status(student_id: &str) -> Result<Value, reqwest::Error> {
    let client = Client::new();
    let url = format!("{}/finances/{}", API_BASE_URL, student_id);
    let response = client.get(&url).send().await?;
    let data = response.json::<Value>().await?;
    Ok(data)
}
