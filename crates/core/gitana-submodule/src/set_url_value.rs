/// One set-URL config assignment.
pub struct SetUrlValue<'a> {
	/// Config section name.
	pub section: &'a str,
	/// Config subsection name.
	pub subsection: &'a str,
	/// New URL value.
	pub url: &'a str,
}
