use caesura::naming::{Shortener, SourceName};
use caesura::source::Metadata;

#[test]
fn get_shortened() {
    // Arrange
    let metadata = Metadata {
        artist: "Artist Name".to_owned(),
        album: "This is a Long Title (With an Even Longer Paranthetical Statement)".to_owned(),
        remaster_title: "Remaster Title".to_owned(),
        year: 1234,
        media: "Vinyl".to_owned(),
    };

    // Act
    let result = Shortener::shorten_album(&metadata);

    // Assert
    let name_before = SourceName::get(&metadata);
    let name_after = SourceName::get(&result.expect("Should have value"));
    assert!(name_after.len() < name_before.len());
}
