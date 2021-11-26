pub enum Message {
    Notify(String),
    AddBook(Vec<u8>),
    Exit,
}
