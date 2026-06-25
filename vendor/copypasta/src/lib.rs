use std::error::Error;

pub trait ClipboardProvider: Sized {
    fn new() -> Result<Self, Box<dyn Error>>;
    fn get_contents(&mut self) -> Result<String, Box<dyn Error>>;
    fn set_contents(&mut self, data: String) -> Result<(), Box<dyn Error>>;
}

pub struct ClipboardContext(arboard::Clipboard);

impl ClipboardProvider for ClipboardContext {
    fn new() -> Result<Self, Box<dyn Error>> {
        Ok(ClipboardContext(arboard::Clipboard::new()?))
    }

    fn get_contents(&mut self) -> Result<String, Box<dyn Error>> {
        Ok(self.0.get_text()?)
    }

    fn set_contents(&mut self, data: String) -> Result<(), Box<dyn Error>> {
        Ok(self.0.set_text(data)?)
    }
}
