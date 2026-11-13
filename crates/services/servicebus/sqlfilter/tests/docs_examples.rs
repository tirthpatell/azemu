// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
//! Filters taken from Microsoft's SQL filter, rule action and filter-example
//! documentation, evaluated against matching and non-matching messages.
mod common;
use common::*;

// "Filter on system properties"
t!(doc_sys_label_like, "sys.Label LIKE '%bus%'", props().sys("Label", "servicebus"), true);
t!(doc_sys_label_like_miss, "sys.Label LIKE '%bus%'", props().sys("Label", "queue"), false);
t!(doc_sys_messageid, "sys.messageid = 'xxxx'", props().sys("MessageId", "xxxx"), true);
t!(doc_sys_correlationid_like, "sys.correlationid like 'abc-%'", props().sys("CorrelationId", "abc-1"), true);
t!(doc_sys_to_in, "sys.To IN ('Ireland','Italy')", props().sys("To", "Italy"), true);
t!(doc_sys_to_in_miss, "sys.To IN ('Ireland','Italy')", props().sys("To", "France"), false);

// "Filter on message properties"
t!(doc_user_equals, "MessageProperty = 'A'", props().user("MessageProperty", "A"), true);
t!(doc_user_like, "user.SuperHero like 'SuperMan%'", props().user("SuperHero", "SuperMan Returns"), true);
t!(doc_price_and_color, "user.price > 10 AND user.color = 'red'", props().user("price", 11).user("color", "red"), true);
t!(doc_price_and_color_miss, "user.price > 10 AND user.color = 'red'", props().user("price", 11).user("color", "blue"), false);
t!(doc_exists, "EXISTS(user.x)", props().user("x", 1), true);
t!(doc_exists_miss, "EXISTS(user.x)", props(), false);
t!(doc_not_parens, "NOT (a = 1)", props().user("a", 2), true);
t!(doc_price_times_two, "price * 2 > 100", props().user("price", 60), true);
t!(doc_price_times_two_miss, "price * 2 > 100", props().user("price", 50), false);
t!(doc_param_style, "a = 1", props().user("a", 1), true);

// "Filter on message properties with numeric values"
t!(doc_num_eq, "MessageProperty = 1", props().user("MessageProperty", 1), true);
t!(doc_num_gt, "MessageProperty > 1", props().user("MessageProperty", 2), true);
t!(doc_num_gt_decimal, "MessageProperty > 2.08", props().user("MessageProperty", 2.1), true);
t!(doc_num_and, "MessageProperty = 1 AND MessageProperty2 = 3", props().user("MessageProperty", 1).user("MessageProperty2", 3), true);
t!(doc_num_or, "MessageProperty = 1 OR MessageProperty2 = 3", props().user("MessageProperty", 5).user("MessageProperty2", 3), true);

// "Using IN and NOT IN"
t!(doc_in_store, "StoreId IN('Store1', 'Store2', 'Store3')", props().user("StoreId", "Store2"), true);
t!(doc_in_or, "sys.To IN ('Store5','Store6','Store7') OR StoreId = 'Store8'", props().user("StoreId", "Store8"), true);
t!(doc_not_in_long,
   "sys.To NOT IN ('Store1','Store2','Store3','Store4','Store5','Store6','Store7','Store8') OR StoreId NOT IN ('Store1','Store2','Store3','Store4','Store5','Store6','Store7','Store8')",
   props().sys("To", "Store9").user("StoreId", "Store1"), true);

// "Filter on message properties with special characters"
t!(doc_special_order_type, "[Order Type] = 'priority'", props().user("Order Type", "priority"), true);
t!(doc_special_order_type_miss, "[Order Type] = 'priority'", props().user("Order Type", "standard"), false);
t!(doc_special_hr, "user.[HR-EmployeeID] = 'E123'", props().user("HR-EmployeeID", "E123"), true);
t!(doc_special_hr_miss, "user.[HR-EmployeeID] = 'E123'", props().user("HR-EmployeeID", "E124"), false);
t!(
    doc_special_quoted_url,
    "\"http://schemas.microsoft.com/xrm/2011/Claims/EntityLogicalName\" = 'account'",
    props().user("http://schemas.microsoft.com/xrm/2011/Claims/EntityLogicalName", "account"),
    true
);

// "Match strings with wildcards" table
t!(doc_w1_a, "user.Code LIKE 'order-%'", props().user("Code", "order-"), true);
t!(doc_w1_b, "user.Code LIKE 'order-%'", props().user("Code", "order-123"), true);
t!(doc_w1_c, "user.Code LIKE 'order-%'", props().user("Code", "preorder-123"), false);
t!(doc_w2_a, "user.Code LIKE '%-complete'", props().user("Code", "-complete"), true);
t!(doc_w3_a, "user.Code LIKE '%order%'", props().user("Code", "preorder-123"), true);
t!(doc_w3_b, "user.Code LIKE '%order%'", props().user("Code", "invoice-123"), false);
t!(doc_w4_a, "user.Code LIKE '%'", props().user("Code", ""), true);
t!(doc_w4_missing, "user.Code LIKE '%'", props(), false);
t!(doc_w5_missing_not_like, "user.Code NOT LIKE 'test-%'", props(), false);
t!(doc_w5_not_like_prefix_only, "user.Code NOT LIKE 'test-%'", props().user("Code", "test-"), false);

// "Filter on string values with special characters"
t!(doc_obrien, "user.CustomerName = 'O''Brien'", props().user("CustomerName", "O'Brien"), true);
t!(doc_obrien_miss, "user.CustomerName = 'O''Brien'", props().user("CustomerName", "OBrien"), false);
t!(
    doc_north_south,
    "user.CustomerName = 'North & South - Retail/Online'",
    props().user("CustomerName", "North & South - Retail/Online"),
    true
);

// "Combine string and numeric conditions"
const COMBINED: &str = "(user.Code LIKE 'order-%' OR user.Code LIKE 'return-%') AND user.Quantity >= 10";
t!(doc_combined_1, COMBINED, props().user("Code", "order-123").user("Quantity", 10), true);
t!(doc_combined_2, COMBINED, props().user("Code", "return-123").user("Quantity", 15), true);
t!(doc_combined_3, COMBINED, props().user("Code", "order-123").user("Quantity", 9), false);
t!(doc_combined_4, COMBINED, props().user("Code", "return-123").user("Quantity", 9), false);
t!(doc_combined_5, COMBINED, props().user("Code", "invoice-123").user("Quantity", 15), false);

// .NET sample filters
t!(doc_sample_blue_size10, "color='blue' AND quantity=10", props().user("color", "blue").user("quantity", 10), true);
t!(doc_sample_red, "user.color='red'", props().user("color", "red"), true);
t!(
    doc_sample_correlation_equivalent,
    "sys.ReplyTo = 'johndoe@contoso.com' AND sys.Label = 'Important' AND color = 'Red'",
    props().sys("ReplyTo", "johndoe@contoso.com").sys("Label", "Important").user("color", "Red"),
    true
);

// Parameter-based filters (timestamps stand in for DateTime; TimeSpan is milliseconds).
#[test]
fn doc_datetime_parameter_filters() {
    let p = props()
        .user("DateTimeMp", Value::Timestamp(1_000))
        .user("DateTimeMp1", Value::Timestamp(1_000))
        .user("DateTimeMp2", Value::Timestamp(4_000));
    let ps = params(&[("dtParam", Value::Timestamp(2_000)), ("timespan", Value::Long(3_000))]);
    assert!(m_with("DateTimeMp < @dtParam", &p, &ps));
    assert!(!m_with("DateTimeMp > @dtParam", &p, &ps));
    assert!(m_with("(DateTimeMp2-DateTimeMp1) <= @timespan", &p, &ps));
    assert!(m_with("DateTimeMp2-DateTimeMp1 <= @timespan", &p, &ps));
}

// The ESCAPE example from the grammar page.
t!(doc_escape_example, "property LIKE 'ABC\\%' ESCAPE '\\'", props().user("property", "ABC%"), true);
t!(doc_escape_example_not_prefix, "property LIKE 'ABC\\%' ESCAPE '\\'", props().user("property", "ABCD"), false);

// Regular identifier grammar and constants from the grammar page.
t!(
    doc_constants,
    "a = 1894 AND b = 2 AND c = 1894.1204 AND d = 2.0 AND e = 101.5E5 AND f = 0.5E-2",
    props().user("a", 1894).user("b", 2).user("c", 1894.1204).user("d", 2.0).user("e", 10150000.0).user("f", 0.005),
    true
);
