use rocket::serde::{Deserialize, Serialize, json::Json};
use rocket::{Build, Rocket, get, launch, routes};

#[derive(Deserialize, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "camelCase", deny_unknown_fields)]
struct DateRange {
    start_date: MinuteDateTime,
    #[serde(
        default,
        deserialize_with = "deserialize_optional",
        skip_serializing_if = "Option::is_none"
    )]
    end_date: Option<MinuteDateTime>,
    symbol: String,
}

fn deserialize_optional<'de, T: Deserialize<'de>, D: rocket::serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde", transparent)]
struct MinuteDateTime(String);

impl<'de> Deserialize<'de> for MinuteDateTime {
    fn deserialize<D: rocket::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let bytes = value.as_bytes();
        let valid = bytes.len() == 16
            && bytes.iter().enumerate().all(|(i, byte)| match i {
                4 | 7 => *byte == b'-',
                10 => *byte == b'T',
                13 => *byte == b':',
                _ => byte.is_ascii_digit(),
            })
            && &bytes[11..13] <= b"23".as_slice()
            && &bytes[14..16] <= b"59".as_slice();
        if valid {
            Ok(Self(value))
        } else {
            Err(rocket::serde::de::Error::custom(
                "expected YYYY-MM-DDTHH:MM with hours 00-23 and minutes 00-59",
            ))
        }
    }
}

#[rocket::post("/klines", format = "json", data = "<range>")]
fn klines(range: Json<DateRange>) -> Json<DateRange> {
    range
}

#[get("/")]
fn index() -> &'static str {
    "kline-web-provider"
}

#[get("/health")]
fn health() -> &'static str {
    "ok"
}

#[launch]
fn rocket() -> Rocket<Build> {
    rocket::build().mount("/", routes![index, health, klines])
}

#[cfg(test)]
mod tests {
    use super::rocket;
    use rocket::{
        http::{ContentType, Status},
        local::blocking::Client,
    };

    #[test]
    fn klines_accepts_date_ranges() {
        let client = Client::tracked(rocket()).unwrap();
        for body in [
            r#"{"startDate":"2026-11-16T14:30","endDate":"2026-11-17T18:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30","symbol":"BTCUSDT"}"#,
        ] {
            let response = client
                .post("/klines")
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(response.into_string().unwrap(), body);
        }
    }

    #[test]
    fn klines_rejects_schema_violations() {
        let client = Client::tracked(rocket()).unwrap();
        for body in [
            r#"{"startDate":"2026-11-16T14:30"}"#,
            r#"{"symbol":"BTCUSDT"}"#,
            r#"{"startDate":null,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":123,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30","extra":true,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T24:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:60","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30:00","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30Z","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30","endDate":"invalid","symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30","endDate":null,"symbol":"BTCUSDT"}"#,
            r#"{"startDate":"2026-11-16T14:30","symbol":123}"#,
            r#"{"startDate":"2026-11-16T14:30","symbol":null}"#,
        ] {
            let response = client
                .post("/klines")
                .header(ContentType::JSON)
                .body(body)
                .dispatch();
            assert_eq!(response.status(), Status::UnprocessableEntity, "{body}");
        }
    }
}
