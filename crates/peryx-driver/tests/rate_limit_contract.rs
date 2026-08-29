use peryx_driver::rate_limit::RouteClass;

#[test]
fn route_classes_expose_the_complete_label_catalog() {
    assert_eq!(
        RouteClass::all().map(RouteClass::as_str),
        ["listing", "metadata", "artifact", "upload", "admin", "authentication"]
    );
}
