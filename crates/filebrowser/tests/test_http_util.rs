use bytehive_filebrowser::http_util::{percent_decode, query_param};

#[test]
fn query_param_extracts_value() {
    let query = "a=1&b=hello%20world&c=";
    assert_eq!(query_param(query, "a"), Some("1".to_string()));
    assert_eq!(query_param(query, "b"), Some("hello world".to_string()));
    assert_eq!(query_param(query, "c"), Some("".to_string()));
    assert_eq!(query_param(query, "d"), None);
}

#[test]
fn percent_decode_handles_encoded_chars() {
    assert_eq!(percent_decode("%41%42%43"), "ABC");
    assert_eq!(percent_decode("plus+test"), "plus test");
}
