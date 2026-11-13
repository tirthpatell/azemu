// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
//! Every example with a result from the Microsoft Learn pages for the Cosmos DB NoSQL query
//! language (learn.microsoft.com/azure/cosmos-db/nosql/query/<page>) that needs no data.
//! Generated from the pages; the page name is the first element of each row.
//! Rows whose docs show `undefined` have those properties removed from the expected result.

mod common;
use common::*;
use serde_json::Value;

const EXAMPLES: &[(&str, &str, &str)] = &[
    (
        "abs",
        r####"SELECT VALUE { absoluteNegativeOne: ABS(-1), absoluteZero: ABS(0), absoluteOne: ABS(1) }"####,
        r####"[{"absoluteNegativeOne": 1, "absoluteZero": 0, "absoluteOne": 1}]"####,
    ),
    ("acos", r####"SELECT VALUE { arccosine: ACOS(-1) }"####, r####"[{"arccosine": 3.141592653589793}]"####),
    (
        "ago",
        r####"SELECT VALUE { "case1": AGO("yyyy", 1), "case2": AGO("yyyy", -1), "case3": AGO("ss", 30), "case4": AGO("ms", 4), "case5": AGO("3d"), "case6": AGO("-3year") }"####,
        r####"[{"case1": "2023-10-14T14:20:30.6668888Z", "case2": "2025-10-14T14:20:30.6668888Z", "case3": "2024-10-14T14:20:00.6668888Z", "case4": "2024-10-14T14:20:30.6628888Z", "case5": "2024-10-11T14:20:30.6668888Z", "case6": "2027-10-14T14:20:30.6668888Z"}]"####,
    ),
    (
        "array-avg",
        r####"SELECT VALUE { "case1": ARRAY_AVG([1, 2, 3, 4]), "case2": ARRAY_AVG([1.0, 2.0, 3.3, 4.7]), "case3": ARRAY_AVG([1, -2.7, 3, -4, undefined]), "case4": ARRAY_AVG(['abc', 'ABC', 'aBc', 'AbC']), "case5": ARRAY_AVG([12, 'abc', true, false, null, undefined]) }"####,
        r####"[{"case1": 2.5, "case2": 2.75, "case3": -0.675}]"####,
    ),
    (
        "array-concat",
        r####"SELECT ARRAY_CONCAT([ "backpacks", "daypacks" ], [ "hippacks" ]) AS gearList"####,
        r####"[{"gearList": ["backpacks", "daypacks", "hippacks"]}]"####,
    ),
    (
        "array-contains-all",
        r####"SELECT VALUE { matchesEntireArray: ARRAY_CONTAINS_ALL([1, true, "3", [1,2,3]], 1, true, "3", [1,2,3]), matchesSomeValues: ARRAY_CONTAINS_ALL([1, 2, 3, 4], 2, 3, 4, 5), matchSingleValue: ARRAY_CONTAINS_ALL([1, 2, 3, 4], 1, undefined), noMatches: ARRAY_CONTAINS_ALL([1, 2, 3, 4], 5, 6, 7, 8), emptyArray: ARRAY_CONTAINS_ALL([], 1, 2, 3), noMatchesUndefined: ARRAY_CONTAINS_ALL([1, 2, 3, 4], 5, undefined) }"####,
        r####"[{"matchesEntireArray": true, "matchesSomeValues": false, "noMatches": false, "emptyArray": false, "noMatchesUndefined": false}]"####,
    ),
    (
        "array-contains-any",
        r####"SELECT VALUE { matchesEntireArray: ARRAY_CONTAINS_ANY([1, true, "3", [1,2,3]], 1, true, "3", [1,2,3]), matchesSomeValues: ARRAY_CONTAINS_ANY([1, 2, 3, 4], 2, 3, 4, 5), matchSingleValue: ARRAY_CONTAINS_ANY([1, 2, 3, 4], 1, undefined), noMatches: ARRAY_CONTAINS_ANY([1, 2, 3, 4], 5, 6, 7, 8), emptyArray: ARRAY_CONTAINS_ANY([], 1, 2, 3), noMatchesUndefined: ARRAY_CONTAINS_ANY([1, 2, 3, 4], 5, undefined) }"####,
        r####"[{"emptyArray": false, "matchSingleValue": true, "matchesEntireArray": true, "matchesSomeValues": true, "noMatches": false}]"####,
    ),
    (
        "array-contains",
        r####"SELECT VALUE { containsItem: ARRAY_CONTAINS(["coats", "jackets", "sweatshirts"], "coats"), missingItem: ARRAY_CONTAINS(["coats", "jackets", "sweatshirts"], "hoodies"), containsFullMatchObject: ARRAY_CONTAINS([{ category: "shirts", color: "blue" }], { category: "shirts", color: "blue" }), missingFullMatchObject: ARRAY_CONTAINS([{ category: "shirts", color: "blue" }], { category: "shirts" }), containsPartialMatchObject: ARRAY_CONTAINS([{ category: "shirts", color: "blue" }], { category: "shirts" }, true), missingPartialMatchObject: ARRAY_CONTAINS([{ category: "shirts", color: "blue" }], { category: "shorts", color: "blue" }, true) }"####,
        r####"[{"containsItem": true, "missingItem": false, "containsFullMatchObject": true, "missingFullMatchObject": false, "containsPartialMatchObject": true, "missingPartialMatchObject": false}]"####,
    ),
    (
        "array-length",
        r####"SELECT VALUE { length: ARRAY_LENGTH([70, 86, 92, 99, 85, 90, 82]), emptyLength: ARRAY_LENGTH([]), nullLength: ARRAY_LENGTH(null) }"####,
        r####"[{"length": 7, "emptyLength": 0}]"####,
    ),
    (
        "array-max",
        r####"SELECT VALUE { "case1": ARRAY_MAX([1, 2, 3, 4]), "case2": ARRAY_MAX(['abc', 'ABC', 'aBc', 'AbC']), "case3": ARRAY_MAX([true, false]), "case4": ARRAY_MAX([null, null]), "case5": ARRAY_MAX([12, 'abc', true, false, null, undefined]) }"####,
        r####"[{"case1": 4, "case2": "abc", "case3": true, "case4": null, "case5": "abc"}]"####,
    ),
    (
        "array-median",
        r####"SELECT VALUE { "case1": ARRAY_MEDIAN([1, 2, 3, 4]), "case2": ARRAY_MEDIAN([1.0, 2.0, 3.3, 4.7, 7.8]), "case3": ARRAY_MEDIAN([1, -2.7, 3, -4, undefined]), "case4": ARRAY_MEDIAN(['abc', 'ABC', 'aBc', 'AbC']), "case5": ARRAY_MEDIAN([12, 'abc', true, false, null, undefined]) }"####,
        r####"[{"case1": 2.5, "case2": 3.3, "case3": -0.8500000000000001}]"####,
    ),
    (
        "array-min",
        r####"SELECT VALUE { "case1": ARRAY_MIN([1, 2, 3, 4]), "case2": ARRAY_MIN(['abc', 'ABC', 'aBc', 'AbC']), "case3": ARRAY_MIN([true, false]), "case4": ARRAY_MIN([null, null]), "case5": ARRAY_MIN([12, 'abc', true, false, null, undefined]) }"####,
        r####"[{"case1": 1, "case2": "ABC", "case3": false, "case4": null, "case5": null}]"####,
    ),
    (
        "array-slice",
        r####"SELECT VALUE { sliceFromStart: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], 0), sliceFromSecond: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], 1), sliceFromLast: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], -1), sliceFromSecondToLast: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], -2), sliceThreeFromStart: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], 0, 3), sliceTwelveFromStart: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], 0, 12), sliceFiveFromThird: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], 3, 5), sliceOneFromSecondToLast: ARRAY_SLICE(["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], -2, 1) }"####,
        r####"[{"sliceFromStart": ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], "sliceFromSecond": ["Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], "sliceFromLast": ["Golf"], "sliceFromSecondToLast": ["Foxtrot", "Golf"], "sliceThreeFromStart": ["Alpha", "Bravo", "Charlie"], "sliceTwelveFromStart": ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf"], "sliceFiveFromThird": ["Delta", "Echo", "Foxtrot", "Golf"], "sliceOneFromSecondToLast": ["Foxtrot"]}]"####,
    ),
    (
        "array-sum",
        r####"SELECT VALUE { "case1": ARRAY_SUM([1, 2, 3, 4]), "case2": ARRAY_SUM([1, 2, 3, 4, undefined]), "case3": ARRAY_SUM(['abc', 'ABC', 'aBc', 'AbC']), "case4": ARRAY_SUM([12, 'abc', true, false, null, undefined]) }"####,
        r####"[{"case1": 10, "case2": 10}]"####,
    ),
    ("asin", r####"SELECT VALUE { arcsine: ACOS(-1) }"####, r####"[{"arcsine": 3.141592653589793}]"####),
    ("atan", r####"SELECT VALUE { arctangent: ATAN(-45.01) }"####, r####"[{"arctangent": -1.5485826962062663}]"####),
    (
        "atn2",
        r####"SELECT VALUE { arctangentInRadians: ATN2(35.175643, 129.44) }"####,
        r####"[{"arctangentInRadians": 0.265344532064832}]"####,
    ),
    (
        "ceiling",
        r####"SELECT VALUE { ceilingPositiveInteger: CEILING(123.45), ceilingNegativeInteger: CEILING(-45.72), ceilingZero: CEILING(0) }"####,
        r####"[{"ceilingPositiveInteger": 124, "ceilingNegativeInteger": -45, "ceilingZero": 0}]"####,
    ),
    ("choose", r####"SELECT VALUE CHOOSE(1, "Vimero", "Hydration", "Pack")"####, r####"["Vimero"]"####),
    (
        "choose",
        r####"SELECT VALUE { index_0: CHOOSE(0, "Mt.", "Hood", "Hydration", "Pack"), index_1: CHOOSE(1, "Mt.", "Hood", "Hydration", "Pack"), index_2: CHOOSE(2, "Mt.", "Hood", "Hydration", "Pack"), index_3: CHOOSE(3, "Mt.", "Hood", "Hydration", "Pack"), index_4: CHOOSE(4, "Mt.", "Hood", "Hydration", "Pack"), index_5: CHOOSE(5, "Mt.", "Hood", "Hydration", "Pack") }"####,
        r####"[{"index_1": "Mt.", "index_2": "Hood", "index_3": "Hydration", "index_4": "Pack"}]"####,
    ),
    ("concat", r####"SELECT VALUE CONCAT("Ferpal", "Ropes")"####, r####"["FerpalRopes"]"####),
    (
        "contains-all-ci",
        r####"SELECT VALUE { "case1": CONTAINS_ALL_CI("Have a nice day!", "have", "nice", "day!"), "case2": CONTAINS_ALL_CI("Have a nice day!", "HAVE", "NICE", "DAY!"), "case3": CONTAINS_ALL_CI("Have a nice day!", "had", "nice", "day!"), "case4": CONTAINS_ALL_CI("Have a nice day!", undefined, "nice", "day!"), "case5": CONTAINS_ALL_CI("Have a nice day!", undefined, "had") }"####,
        r####"[{"case1": true, "case2": true, "case3": false, "case5": false}]"####,
    ),
    (
        "contains-all-cs",
        r####"SELECT VALUE { "case1": CONTAINS_ALL_CS("Have a nice day!", "have", "nice", "day!"), "case2": CONTAINS_ALL_CS("Have a nice day!", "HAVE", "NICE", "DAY!"), "case3": CONTAINS_ALL_CS("Have a nice day!", "had", "nice", "day!"), "case4": CONTAINS_ALL_CS("Have a nice day!", undefined, "nice", "day!"), "case5": CONTAINS_ALL_CS("Have a nice day!", undefined, "had") }"####,
        r####"[{"case1": false, "case2": false, "case3": false, "case5": false}]"####,
    ),
    (
        "contains-any-ci",
        r####"SELECT VALUE { "case1": CONTAINS_ANY_CI("Have a nice day!", "have", "nice", "day!"), "case2": CONTAINS_ANY_CI("Have a nice day!", "HAVE", "NICE", "DAY!"), "case3": CONTAINS_ANY_CI("Have a nice day!", "had", "nice", "day!"), "case4": CONTAINS_ANY_CI("Have a nice day!", undefined, "nice", "day!"), "case5": CONTAINS_ANY_CI("Have a nice day!", undefined, "had") }"####,
        r####"[{"case1": true, "case2": true, "case3": true, "case4": true}]"####,
    ),
    (
        "contains-any-cs",
        r####"SELECT VALUE { "case1": CONTAINS_ANY_CS("Have a nice day!", "have", "nice", "day!"), "case2": CONTAINS_ANY_CS("Have a nice day!", "HAVE", "NICE", "DAY!"), "case3": CONTAINS_ANY_CS("Have a nice day!", "had", "nice", "day!"), "case4": CONTAINS_ANY_CS("Have a nice day!", undefined, "nice", "day!"), "case5": CONTAINS_ANY_CS("Have a nice day!", undefined, "had") }"####,
        r####"[{"case1": true, "case2": false, "case3": true, "case4": true}]"####,
    ),
    (
        "contains",
        r####"SELECT VALUE { containsPrefix: CONTAINS("AdventureWorks", "Adventure"), containsSuffix: CONTAINS("AdventureWorks", "Works"), containsWrongCase: CONTAINS("AdventureWorks", "adventure"), containsWrongCaseValidateCase: CONTAINS("AdventureWorks", "adventure", false), containsWrongCaseIgnoreCase: CONTAINS("AdventureWorks", "works", true), containsMismatch: CONTAINS("AdventureWorks", "Contoso") }"####,
        r####"[{"containsPrefix": true, "containsSuffix": true, "containsWrongCase": false, "containsWrongCaseValidateCase": false, "containsWrongCaseIgnoreCase": true, "containsMismatch": false}]"####,
    ),
    ("cos", r####"SELECT VALUE { cosine: COS(14.78) }"####, r####"[{"cosine": -0.5994654261946543}]"####),
    ("cot", r####"SELECT VALUE { cotangent: COT(124.1332) }"####, r####"[{"cotangent": -0.040311998371148884}]"####),
    (
        "count",
        r####"SELECT VALUE { countScalar: COUNT(1), countExpression: COUNT(2 + 3) }"####,
        r####"[{"countScalar": 1, "countExpression": 1}]"####,
    ),
    (
        "datetimeadd",
        r####"SELECT VALUE { addOneYear: DATETIMEADD("yyyy", 1, "2020-07-03T00:00:00.0000000"), addOneMonth: DATETIMEADD("mm", 1, "2020-07-03T00:00:00.0000000"), addOneDay: DATETIMEADD("dd", 1, "2020-07-03T00:00:00.0000000"), addOneHour: DATETIMEADD("hh", 1, "2020-07-03T00:00:00.0000000"), subtractOneYear: DATETIMEADD("yyyy", -1, "2020-07-03T00:00:00.0000000"), subtractOneMonth: DATETIMEADD("mm", -1, "2020-07-03T00:00:00.0000000"), subtractOneDay: DATETIMEADD("dd", -1, "2020-07-03T00:00:00.0000000"), subtractOneHour: DATETIMEADD("hh", -1, "2020-07-03T00:00:00.0000000"), modifySecondsExpression: DATETIMEADD("ss", 5 * -5, "2020-07-03T00:00:00.0000000") }"####,
        r####"[{"addOneYear": "2021-07-03T00:00:00.0000000Z", "addOneMonth": "2020-08-03T00:00:00.0000000Z", "addOneDay": "2020-07-04T00:00:00.0000000Z", "addOneHour": "2020-07-03T01:00:00.0000000Z", "subtractOneYear": "2019-07-03T00:00:00.0000000Z", "subtractOneMonth": "2020-06-03T00:00:00.0000000Z", "subtractOneDay": "2020-07-02T00:00:00.0000000Z", "subtractOneHour": "2020-07-02T23:00:00.0000000Z", "modifySecondsExpression": "2020-07-02T23:59:35.0000000Z"}]"####,
    ),
    (
        "datetimebin",
        r####"SELECT VALUE { binDay: DATETIMEBIN("2021-01-08T18:35:00.0000000", "dd"), binHour: DATETIMEBIN("2021-01-08T18:35:00.0000000", "hh"), binSecond: DATETIMEBIN("2021-01-08T18:35:00.0000000", "ss"), binFiveHours: DATETIMEBIN("2021-01-08T18:35:00.0000000", "hh", 5), binSevenDaysUnixEpoch: DATETIMEBIN("2021-01-08T18:35:00.0000000", "dd", 7), binSevenDaysWindowsEpoch: DATETIMEBIN("2021-01-08T18:35:00.0000000", "dd", 7, "1601-01-01T00:00:00.0000000") }"####,
        r####"[{"binDay": "2021-01-08T00:00:00.0000000Z", "binHour": "2021-01-08T18:00:00.0000000Z", "binSecond": "2021-01-08T18:35:00.0000000Z", "binFiveHours": "2021-01-08T15:00:00.0000000Z", "binSevenDaysUnixEpoch": "2021-01-07T00:00:00.0000000Z", "binSevenDaysWindowsEpoch": "2021-01-04T00:00:00.0000000Z"}]"####,
    ),
    (
        "datetimediff",
        r####"SELECT VALUE { diffPastYears: DATETIMEDIFF("yyyy", "2019-02-04T16:00:00.0000000", "2018-03-05T05:00:00.0000000"), diffPastMonths: DATETIMEDIFF("mm", "2019-02-04T16:00:00.0000000", "2018-03-05T05:00:00.0000000"), diffPastDays: DATETIMEDIFF("dd", "2019-02-04T16:00:00.0000000", "2018-03-05T05:00:00.0000000"), diffPastHours: DATETIMEDIFF("hh", "2019-02-04T16:00:00.0000000", "2018-03-05T05:00:00.0000000"), diffPastSeconds: DATETIMEDIFF("ss", "2019-02-04T16:00:00.0000000", "2018-03-05T05:00:00.0000000"), diffFutureYears: DATETIMEDIFF("yyyy", "2018-03-05T05:00:00.0000000", "2019-02-04T16:00:00.0000000"), diffFutureMonths: DATETIMEDIFF("mm", "2018-03-05T05:00:00.0000000", "2019-02-04T16:00:00.0000000"), diffFutureDays: DATETIMEDIFF("dd", "2018-03-05T05:00:00.0000000", "2019-02-04T16:00:00.0000000"), diffFutureHours: DATETIMEDIFF("hh", "2018-03-05T05:00:00.0000000", "2019-02-04T16:00:00.0000000"), diffFutureSeconds: DATETIMEDIFF("ss", "2018-03-05T05:00:00.0000000", "2019-02-04T16:00:00.0000000") }"####,
        r####"[{"diffPastYears": -1, "diffPastMonths": -11, "diffPastDays": -336, "diffPastHours": -8075, "diffPastSeconds": -29070000, "diffFutureYears": 1, "diffFutureMonths": 11, "diffFutureDays": 336, "diffFutureHours": 8075, "diffFutureSeconds": 29070000}]"####,
    ),
    (
        "datetimeformat",
        r####"SELECT VALUE { "case1": DATETIMEFORMAT("2024-10-10 14:40:20", "yyyyMMdd"), "case2": DATETIMEFORMAT("2024-10-10 14:40:20", "yyyy-MM-dd [HH:mm:ss]"), "case3": DATETIMEFORMAT("2024-10-10 14:40:20", "To\\da\\y i\\s: yyyy-MM-ddTHH:mm:ss.fff"), "case4": DATETIMEFORMAT("2024-10-10 14:40:20", "To\\da\\y i\\s: yyyy-MM-ddTHH:mm:ss.FFF") }"####,
        r####"[{"case1": "20241010", "case2": "2024-10-10 [14:40:20]", "case3": "Today is: 2024-10-10T14:40:20.000", "case4": "Today is: 2024-10-10T14:40:20."}]"####,
    ),
    (
        "datetimefromparts",
        r####"SELECT VALUE { constructMinArguments: DATETIMEFROMPARTS(2017, 4, 20), constructMinEquivalent: DATETIMEFROMPARTS(2017, 4, 20, 0, 0, 0, 0), constructAllArguments: DATETIMEFROMPARTS(2017, 4, 20, 13, 15, 20, 3456789), constructPartialArguments: DATETIMEFROMPARTS(2017, 4, 20, 13, 15), constructInvalidArguments: DATETIMEFROMPARTS(-2000, -1, -1) }"####,
        r####"[{"constructMinArguments": "2017-04-20T00:00:00.0000000Z", "constructMinEquivalent": "2017-04-20T00:00:00.0000000Z", "constructAllArguments": "2017-04-20T13:15:20.3456789Z", "constructPartialArguments": "2017-04-20T13:15:00.0000000Z"}]"####,
    ),
    (
        "datetimepart",
        r####"SELECT VALUE { getYear: DATETIMEPART("yyyy", "2016-05-29T08:30:00.1301617"), getMonth: DATETIMEPART("mm", "2016-05-29T08:30:00.1301617"), getDay: DATETIMEPART("dd", "2016-05-29T08:30:00.1301617"), getHour: DATETIMEPART("hh", "2016-05-29T08:30:00.1301617"), getMinute: DATETIMEPART("mi", "2016-05-29T08:30:00.1301617"), getSecond: DATETIMEPART("ss", "2016-05-29T08:30:00.1301617"), getMillisecond: DATETIMEPART("ms", "2016-05-29T08:30:00.1301617"), getMicrosecond: DATETIMEPART("mcs", "2016-05-29T08:30:00.1301617"), getNanosecond: DATETIMEPART("ns", "2016-05-29T08:30:00.1301617") }"####,
        r####"[{"getYear": 2016, "getMonth": 5, "getDay": 29, "getHour": 8, "getMinute": 30, "getSecond": 0, "getMillisecond": 130, "getMicrosecond": 130161, "getNanosecond": 130161700}]"####,
    ),
    (
        "datetimetoticks",
        r####"SELECT VALUE { ticks: DATETIMETOTICKS("2015-05-19T12:00:00.0000000") }"####,
        r####"[{"ticks": 14320368000000000}]"####,
    ),
    (
        "datetimetotimestamp",
        r####"SELECT VALUE { timestamp: DATETIMETOTIMESTAMP("2015-05-19T12:00:00.0000000") }"####,
        r####"[{"timestamp": 1432036800000}]"####,
    ),
    (
        "day",
        r####"SELECT VALUE { "case1": DAY("2024-01-10"), "case2": DAY("2000-12-12T10:00:00"), "case3": DAY("1989-03-03T12:12:12.1234567Z") }"####,
        r####"[{"case1": 10, "case2": 12, "case3": 3}]"####,
    ),
    (
        "degrees",
        r####"SELECT VALUE { radiansHalfPiToDegrees: DEGREES(1.5707963267948966), radiansPiToDegrees: DEGREES(3.141592653589793), radiansOneAndHalfPiToDegrees: DEGREES(4.71238898038469), radiansDoublePiToDegrees: DEGREES(6.283185307179586) }"####,
        r####"[{"radiansHalfPiToDegrees": 90, "radiansPiToDegrees": 180, "radiansOneAndHalfPiToDegrees": 270, "radiansDoublePiToDegrees": 360}]"####,
    ),
    (
        "endswith",
        r####"SELECT VALUE { endsWithWrongSuffix: ENDSWITH("AdventureWorks", "Adventure"), endsWithCorrectSuffix: ENDSWITH("AdventureWorks", "Works"), endsWithSuffixWrongCase: ENDSWITH("AdventureWorks", "works"), endsWithSuffixCaseInsensitive: ENDSWITH("AdventureWorks", "works", true) }"####,
        r####"[{"endsWithWrongSuffix": false, "endsWithCorrectSuffix": true, "endsWithSuffixWrongCase": false, "endsWithSuffixCaseInsensitive": true}]"####,
    ),
    (
        "exp",
        r####"SELECT VALUE { expontentialZero: EXP(0), exponentialTen: EXP(10), exponentialTwenty: EXP(20) }"####,
        r####"[{"expontentialZero": 1, "exponentialTen": 22026.465794806718, "exponentialTwenty": 485165195.4097903}]"####,
    ),
    (
        "floor",
        r####"SELECT VALUE { floorPostiveNumber: FLOOR(62.6), floorNegativeNumber: FLOOR(-145.12), floorSmallNumber: FLOOR(0.2989), floorZero: FLOOR(0.0), floorNull: FLOOR(null) }"####,
        r####"[{"floorPostiveNumber": 62, "floorNegativeNumber": -146, "floorSmallNumber": 0, "floorZero": 0}]"####,
    ),
    (
        "iif",
        r####"SELECT VALUE { evalTrue: IIF(true, 123, 456), evalFalse: IIF(false, 123, 456), evalNumberNotTrue: IIF(123, 123, 456), evalStringNotTrue: IIF("ABC", 123, 456), evalArrayNotTrue: IIF([1,2,3], 123, 456), evalObjectNotTrue: IIF({"name": "Alice", "age": 20}, 123, 456) }"####,
        r####"[{"evalTrue": 123, "evalFalse": 456, "evalNumberNotTrue": 456, "evalStringNotTrue": 456, "evalArrayNotTrue": 456, "evalObjectNotTrue": 456}]"####,
    ),
    (
        "index-of",
        r####"SELECT VALUE { indexOfFirstLetter: INDEX_OF("AdventureWorks", "A"), indexOfLastLetter: INDEX_OF("AdventureWorks", "s"), indexOfPrefix: INDEX_OF("AdventureWorks", "Adventure"), indexOfSuffix: INDEX_OF("AdventureWorks", "Works"), indexOfSubstring: INDEX_OF("AdventureWorks", "tureW"), indexOfNonMatch: INDEX_OF("AdventureWorks", "Cosmos"), indexOfCustomStartMatch: INDEX_OF("AdventureWorks", "Works", 5), indexOfCustomStartNoMatch: INDEX_OF("AdventureWorks", "Adventure", 5), indexOfCaseSensitive: INDEX_OF("AdventureWorks", "aD") }"####,
        r####"[{"indexOfFirstLetter": 0, "indexOfLastLetter": 13, "indexOfPrefix": 0, "indexOfSuffix": 9, "indexOfSubstring": 5, "indexOfNonMatch": -1, "indexOfCustomStartMatch": 9, "indexOfCustomStartNoMatch": -1, "indexOfCaseSensitive": -1}]"####,
    ),
    (
        "intadd",
        r####"SELECT VALUE { addNumber: INTADD(20, 10), addZero: INTADD(20, 0), addDecimal: INTADD(20, 0.10) }"####,
        r####"[{"addNumber": 30, "addZero": 20}]"####,
    ),
    (
        "intbitand",
        r####"SELECT VALUE { compareNumbers: INTBITAND(15, 25), compareZero: INTBITAND(15, 0), compareSameNumber: INTBITAND(15, 15), compareDecimal: INTBITAND(15, 1.5) }"####,
        r####"[{"compareNumbers": 9, "compareZero": 0, "compareSameNumber": 15}]"####,
    ),
    (
        "intbitleftshift",
        r####"SELECT VALUE { shiftInteger: INTBITLEFTSHIFT(16, 4), shiftDecimal: INTBITLEFTSHIFT(16, 0.4) }"####,
        r####"[{"shiftInteger": 256}]"####,
    ),
    (
        "intbitnot",
        r####"SELECT VALUE { complementNumber: INTBITNOT(65), complementZero: INTBITNOT(0), complementDecimal: INTBITNOT(0.1) }"####,
        r####"[{"complementNumber": -66, "complementZero": -1}]"####,
    ),
    (
        "intbitor",
        r####"SELECT VALUE { inclusiveOr: INTBITOR(56, 100), inclusiveOrSame: INTBITOR(56, 56), inclusiveOrZero: INTBITOR(56, 0), inclusiveOrDecimal: INTBITOR(56, 0.1) }"####,
        r####"[{"inclusiveOr": 124, "inclusiveOrSame": 56, "inclusiveOrZero": 56}]"####,
    ),
    (
        "intbitrightshift",
        r####"SELECT VALUE { shiftInteger: INTBITRIGHTSHIFT(16, 4), shiftDecimal: INTBITRIGHTSHIFT(16, 0.4) }"####,
        r####"[{"shiftInteger": 1}]"####,
    ),
    (
        "intbitxor",
        r####"SELECT VALUE { exclusiveOr: INTBITXOR(56, 100), exclusiveOrSame: INTBITXOR(56, 56), exclusiveOrZero: INTBITXOR(56, 0), exclusiveOrDecimal: INTBITXOR(56, 0.1) }"####,
        r####"[{"exclusiveOr": 92, "exclusiveOrSame": 0, "exclusiveOrZero": 56}]"####,
    ),
    (
        "intdiv",
        r####"SELECT VALUE { divide: INTDIV(10, 2), negativeResult: INTDIV(10, -2), positiveResult: INTDIV(-10, -2), resultOne: INTDIV(10, 10), divideZero: INTDIV(10, 0), divideDecimal: INTDIV(10, 0.1) }"####,
        r####"[{"divide": 5, "negativeResult": -5, "positiveResult": 5, "resultOne": 1}]"####,
    ),
    (
        "intmod",
        r####"SELECT VALUE { mod: INTMOD(12, 5), positiveResult: INTMOD(12, -5), negativeResult: INTMOD(-12, -5), resultZero: INTMOD(15, 5), modZero: INTMOD(12, 0), modDecimal: INTMOD(12, 0.2) }"####,
        r####"[{"mod": 2, "positiveResult": 2, "negativeResult": -2, "resultZero": 0}]"####,
    ),
    (
        "intmul",
        r####"SELECT VALUE { multiply: INTMUL(5, 2), negativeResult: INTMUL(5, -2), positiveResult: INTMUL(-5, -2), square: INTMUL(5, 5), cube: INTMUL(5, INTMUL(5, 5)), multiplyZero: INTMUL(5, 0), multiplyDecimal: INTMUL(5, 0.5) }"####,
        r####"[{"multiply": 10, "negativeResult": -10, "positiveResult": 10, "square": 25, "cube": 125, "multiplyZero": 0}]"####,
    ),
    (
        "intsub",
        r####"SELECT VALUE { negativeResult: INTSUB(25, 50), positiveResult: INTSUB(25, 15), subtractSameNumber: INTSUB(25, 25), subtractZero: INTSUB(25, 0), subtractDecimal: INTSUB(25, 2.5) }"####,
        r####"[{"negativeResult": -25, "positiveResult": 10, "subtractSameNumber": 0, "subtractZero": 25}]"####,
    ),
    (
        "is-array",
        r####"SELECT VALUE { booleanIsArray: IS_ARRAY(true), numberIsArray: IS_ARRAY(65), stringIsArray: IS_ARRAY("AdventureWorks"), nullIsArray: IS_ARRAY(null), objectIsArray: IS_ARRAY({size: "small"}), arrayIsArray: IS_ARRAY([25344, 82947]), arrayObjectPropertyIsArray: IS_ARRAY({skus: [25344, 82947], vendors: null}.skus), invalidObjectPropertyIsArray: IS_ARRAY({skus: [25344, 82947], vendors: null}.size), nullObjectPropertyIsArray: IS_ARRAY({skus: [25344, 82947], vendors: null}.vendor) }"####,
        r####"[{"booleanIsArray": false, "numberIsArray": false, "stringIsArray": false, "nullIsArray": false, "objectIsArray": false, "arrayIsArray": true, "arrayObjectPropertyIsArray": true, "invalidObjectPropertyIsArray": false, "nullObjectPropertyIsArray": false}]"####,
    ),
    (
        "is-bool",
        r####"SELECT VALUE { booleanIsBool: IS_BOOL(true), numberIsBool: IS_BOOL(65), stringIsBool: IS_BOOL("AdventureWorks"), nullIsBool: IS_BOOL(null), objectIsBool: IS_BOOL({size: "small"}), arrayIsBool: IS_BOOL([25344, 82947]), arrayObjectPropertyIsBool: IS_BOOL({skus: [25344, 82947], vendors: null}.skus), invalidObjectPropertyIsBool: IS_BOOL({skus: [25344, 82947], vendors: null}.size), nullObjectPropertyIsBool: IS_BOOL({skus: [25344, 82947], vendors: null}.vendor) }"####,
        r####"[{"booleanIsBool": true, "numberIsBool": false, "stringIsBool": false, "nullIsBool": false, "objectIsBool": false, "arrayIsBool": false, "arrayObjectPropertyIsBool": false, "invalidObjectPropertyIsBool": false, "nullObjectPropertyIsBool": false}]"####,
    ),
    (
        "is-datetime",
        r####"SELECT VALUE { "case1": IS_DATETIME("2024-12-12"), "case2": IS_DATETIME("2024-12-12Z"), "case3": IS_DATETIME("2024-12-12 20:21:25"), "case4": IS_DATETIME("2024-12-12T20:21:25Z"), "case5": IS_DATETIME("2024-12-12T20:21:25.123"), "case6": IS_DATETIME("2024-12-12 20:21:25.123Z"), "case7": IS_DATETIME("2024-12-12T20Z"), "case8": IS_DATETIME("2024-12-12 20:21:25.") }"####,
        r####"[{"case1": true, "case2": true, "case3": true, "case4": true, "case5": true, "case6": true, "case7": false, "case8": false}]"####,
    ),
    (
        "is-defined",
        r####"SELECT VALUE { isDefined: IS_DEFINED({ "quantity" : 5 }.quantity), isNotDefined: IS_DEFINED({ "quantity" : 5 }.name) }"####,
        r####"[{"isDefined": true, "isNotDefined": false}]"####,
    ),
    (
        "is-finite-number",
        r####"SELECT VALUE { finiteValue: IS_FINITE_NUMBER(1234.567), infiniteValue: IS_FINITE_NUMBER(8.9 / 0.0), nanValue: IS_FINITE_NUMBER(SQRT(-1.0)) }"####,
        r####"[{"finiteValue": true, "infiniteValue": false, "nanValue": false}]"####,
    ),
    (
        "is-integer",
        r####"SELECT VALUE { smallDecimalValue: IS_INTEGER(3454.123), integerValue: IS_INTEGER(5523432), minIntegerValue: IS_INTEGER(-9223372036854775808), maxIntegerValue: IS_INTEGER(9223372036854775807), outOfRangeValue: IS_INTEGER(18446744073709551615) }"####,
        r####"[{"smallDecimalValue": false, "integerValue": true, "minIntegerValue": true, "maxIntegerValue": true, "outOfRangeValue": false}]"####,
    ),
    (
        "is-null",
        r####"SELECT VALUE { booleanIsNull: IS_NULL(true), numberIsNull: IS_NULL(15), stringIsNull: IS_NULL("AdventureWorks"), nullIsNull: IS_NULL(null), objectIsNull: IS_NULL({price: 85.23}), arrayIsNull: IS_NULL(["red", "blue", "yellow"]), populatedObjectPropertyIsNull: IS_NULL({quantity: 25, vendor: null}.quantity), invalidObjectPropertyIsNull: IS_NULL({quantity: 25, vendor: null}.size), nullObjectPropertyIsNull: IS_NULL({quantity: 25, vendor: null}.vendor) }"####,
        r####"[{"booleanIsNull": false, "numberIsNull": false, "stringIsNull": false, "nullIsNull": true, "objectIsNull": false, "arrayIsNull": false, "populatedObjectPropertyIsNull": false, "invalidObjectPropertyIsNull": false, "nullObjectPropertyIsNull": true}]"####,
    ),
    (
        "is-number",
        r####"SELECT VALUE { isBooleanANumber: IS_NUMBER(true), isNumberANumber: IS_NUMBER(1), isStringANumber: IS_NUMBER("value"), isNullANumber: IS_NUMBER(null), isObjectANumber: IS_NUMBER({ "name": "Tecozow coat" }), isObjectStringPropertyANumber: IS_NUMBER({ "name": "Tecozow coat" }.name), isObjectNumberPropertyANumber: IS_NUMBER({ "quantity": 0 }.quantity), isUndefinedANumber: IS_NUMBER({}.category) }"####,
        r####"[{"isBooleanANumber": false, "isNumberANumber": true, "isStringANumber": false, "isNullANumber": false, "isObjectANumber": false, "isObjectStringPropertyANumber": false, "isObjectNumberPropertyANumber": true, "isUndefinedANumber": false}]"####,
    ),
    (
        "is-object",
        r####"SELECT VALUE { isBooleanAnObject: IS_OBJECT(true), isNumberAnObject: IS_OBJECT(1), isStringAnObject: IS_OBJECT("value"), isArrayAnObject: IS_OBJECT([ "green", "red", "yellow" ]), isNullAnObject: IS_OBJECT(null), isObjectAnObject: IS_OBJECT({ "name": "Tecozow coat" }), isObjectStringPropertyAnObject: IS_OBJECT({ "name": "Tecozow coat" }.name), isObjectObjectPropertyAnObject: IS_OBJECT({ "quantity": { "count": 0 } }.quantity), isUndefinedAnObject: IS_OBJECT({}.category) }"####,
        r####"[{"isBooleanAnObject": false, "isNumberAnObject": false, "isStringAnObject": false, "isArrayAnObject": false, "isNullAnObject": false, "isObjectAnObject": true, "isObjectStringPropertyAnObject": false, "isObjectObjectPropertyAnObject": true, "isUndefinedAnObject": false}]"####,
    ),
    (
        "is-primitive",
        r####"SELECT VALUE { isBooleanAPrimitive: IS_PRIMITIVE(true), isNumberAPrimitive: IS_PRIMITIVE(1), isStringAPrimitive: IS_PRIMITIVE("value"), isArrayAPrimitive: IS_PRIMITIVE([ "green", "red", "yellow" ]), isNullAPrimitive: IS_PRIMITIVE(null), isObjectAPrimitive: IS_PRIMITIVE({ "name": "Tecozow coat" }), isObjectStringPropertyAPrimitive: IS_PRIMITIVE({ "name": "Tecozow coat" }.name), isObjectBooleanPropertyAPrimitive: IS_PRIMITIVE({ "onSale": false }.onSale), isUndefinedAPrimitive: IS_PRIMITIVE({}.category) }"####,
        r####"[{"isBooleanAPrimitive": true, "isNumberAPrimitive": true, "isStringAPrimitive": true, "isArrayAPrimitive": false, "isNullAPrimitive": true, "isObjectAPrimitive": false, "isObjectStringPropertyAPrimitive": true, "isObjectBooleanPropertyAPrimitive": true, "isUndefinedAPrimitive": false}]"####,
    ),
    (
        "is-string",
        r####"SELECT VALUE { isBooleanAString: IS_STRING(true), isNumberAString: IS_STRING(1), isStringAString: IS_STRING("value"), isArrayAString: IS_STRING([ "green", "red", "yellow" ]), isNullAString: IS_STRING(null), isObjectAString: IS_STRING({ "name": "Tecozow coat" }), isObjectStringPropertyAString: IS_STRING({ "name": "Tecozow coat" }.name), isObjectBooleanPropertyAString: IS_STRING({ "onSale": false }.onSale), isUndefinedAString: IS_STRING({}.category) }"####,
        r####"[{"isBooleanAString": false, "isNumberAString": false, "isStringAString": true, "isArrayAString": false, "isNullAString": false, "isObjectAString": false, "isObjectStringPropertyAString": true, "isObjectBooleanPropertyAString": false, "isUndefinedAString": false}]"####,
    ),
    (
        "lastindexof",
        r####"SELECT VALUE { "case1": LASTINDEXOF("abcdeabc", "abc"), "case2": LASTINDEXOF("abcdeabc", "abc", 6), "case3": LASTINDEXOF("abcdeabc", "abc", 7), "case4": LASTINDEXOF("abcdeabc", "fg") }"####,
        r####"[{"case1": 5, "case2": 0, "case3": 5, "case4": -1}]"####,
    ),
    (
        "left",
        r####"SELECT VALUE { firstZero: LEFT("AdventureWorks", 0), firstOne: LEFT("AdventureWorks", 1), firstFive: LEFT("AdventureWorks", 5), fullLength: LEFT("AdventureWorks", LENGTH("AdventureWorks")), beyondMaxLength: LEFT("AdventureWorks", 100) }"####,
        r####"[{"firstZero": "", "firstOne": "A", "firstFive": "Adven", "fullLength": "AdventureWorks", "beyondMaxLength": "AdventureWorks"}]"####,
    ),
    (
        "length",
        r####"SELECT VALUE { stringValue: LENGTH("AdventureWorks"), emptyString: LENGTH("") }"####,
        r####"[{"stringValue": 14, "emptyString": 0}]"####,
    ),
    (
        "log",
        r####"SELECT VALUE { logFive: LOG(5), logTwoBaseTen: LOG(2, 10) }"####,
        r####"[{"logFive": 1.6094379124341003, "logTwoBaseTen": 0.3010299956639812}]"####,
    ),
    (
        "log10",
        r####"SELECT VALUE { logFiveBaseTen: LOG10(5), logTwoBaseTen: LOG10(2), logHundredBaseTen: LOG10(100) }"####,
        r####"[{"logFiveBaseTen": 0.6989700043360189, "logTwoBaseTen": 0.3010299956639812, "logHundredBaseTen": 2}]"####,
    ),
    (
        "lower",
        r####"SELECT VALUE { lowercase: LOWER("adventureworks"), uppercase: LOWER("ADVENTUREWORKS"), camelCase: LOWER("adventureWorks"), pascalCase: LOWER("AdventureWorks"), upperSnakeCase: LOWER("ADVENTURE_WORKS") }"####,
        r####"[{"lowercase": "adventureworks", "uppercase": "adventureworks", "camelCase": "adventureworks", "pascalCase": "adventureworks", "upperSnakeCase": "adventure_works"}]"####,
    ),
    (
        "ltrim",
        r####"SELECT VALUE { whitespaceStart: LTRIM("  AdventureWorks"), whitespaceStartEnd: LTRIM("  AdventureWorks  "), whitespaceEnd: LTRIM("AdventureWorks  "), noWhitespace: LTRIM("AdventureWorks"), trimSuffix: LTRIM("AdventureWorks", "Works"), trimPrefix: LTRIM("AdventureWorks", "Adventure"), trimEntireTerm: LTRIM("AdventureWorks", "AdventureWorks"), trimEmptyString: LTRIM("AdventureWorks", "") }"####,
        r####"[{"whitespaceStart": "AdventureWorks", "whitespaceStartEnd": "AdventureWorks  ", "whitespaceEnd": "AdventureWorks  ", "noWhitespace": "AdventureWorks", "trimSuffix": "AdventureWorks", "trimPrefix": "Works", "trimEntireTerm": "", "trimEmptyString": "AdventureWorks"}]"####,
    ),
    (
        "month",
        r####"SELECT VALUE { "case1": MONTH("2024-01-10"), "case2": MONTH("2000-12-12T10:00:00"), "case3": MONTH("1989-03-03T12:12:12.1234567Z") }"####,
        r####"[{"case1": 1, "case2": 12, "case3": 3}]"####,
    ),
    (
        "now",
        r####"SELECT VALUE { "case1": NOW(), "case2": NOW("yyyy", 1), "case3": NOW("yyyy", -1), "case4": NOW("ss", 30), "case5": NOW("ms", 4), "case6": NOW("3d"), "case7": NOW("-3year") }"####,
        r####"[{"case1": "2024-10-14T14:20:30.6668888Z", "case2": "2025-10-14T14:20:30.6668888Z", "case3": "2023-10-14T14:20:30.6668888Z", "case4": "2024-10-14T14:21:00.6668888Z", "case5": "2024-10-14T14:20:30.6708888Z", "case6": "2024-10-17T14:20:30.6668888Z", "case7": "2021-10-14T14:20:30.6668888Z"}]"####,
    ),
    (
        "numberbin",
        r####"SELECT VALUE { roundToNegativeHundreds: NUMBERBIN(37.752, -100), roundToTens: NUMBERBIN(37.752, 10), roundToOnes: NUMBERBIN(37.752, 1), roundToZeroes: NUMBERBIN(37.752, 0), roundToOneTenths: NUMBERBIN(37.752, 0.1), roundToOneHundreds: NUMBERBIN(37.752, 0.01) }"####,
        r####"[{"roundToNegativeHundreds": 100, "roundToTens": 30, "roundToOnes": 37, "roundToOneTenths": 37.7, "roundToOneHundreds": 37.75}]"####,
    ),
    (
        "objecttoarray",
        r####"SELECT VALUE OBJECTTOARRAY({ "a": "12345", "b": "67890" })"####,
        r####"[[{"k": "a", "v": "12345"}, {"k": "b", "v": "67890"}]]"####,
    ),
    ("pi", r####"SELECT VALUE PI()"####, r####"[3.141592653589793]"####),
    (
        "power",
        r####"SELECT VALUE { oneFirstPower: POWER(1, 1), twoSquared: POWER(2, 2), threeCubed: POWER(3, 3), fourFourthPower: POWER(4, 4), fiveFithPower: POWER(5, 5), zeroSquared: POWER(0, 2), nullCubed: POWER(null, 3), twoNullPower: POWER(2, null) }"####,
        r####"[{"oneFirstPower": 1, "twoSquared": 4, "threeCubed": 27, "fourFourthPower": 256, "fiveFithPower": 3125, "zeroSquared": 0}]"####,
    ),
    (
        "radians",
        r####"SELECT VALUE { degrees90ToRadians: RADIANS(90), degrees180ToRadians: RADIANS(180), degrees270ToRadians: RADIANS(270), degrees360ToRadians: RADIANS(360) }"####,
        r####"[{"degrees90ToRadians": 1.5707963267948966, "degrees180ToRadians": 3.141592653589793, "degrees270ToRadians": 4.71238898038469, "degrees360ToRadians": 6.283185307179586}]"####,
    ),
    (
        "regexmatch",
        r####"SELECT VALUE { noModifiers: REGEXMATCH("abcd", "ABC"), caseInsensitive: REGEXMATCH("abcd", "ABC", "i"), wildcardCharacter: REGEXMATCH("abcd", "ab.", ""), ignoreWhiteSpace: REGEXMATCH("abcd", "ab c", "x"), caseInsensitiveAndIgnoreWhiteSpace: REGEXMATCH("abcd", "aB c", "ix"), containNumberBetweenZeroAndNine: REGEXMATCH("03a", "[0-9]"), containPrefix: REGEXMATCH("salt3824908", "salt{1}"), containsFiveLetterWordStartingWithS: REGEXMATCH("shame", "s....", "i") }"####,
        r####"[{"noModifiers": false, "caseInsensitive": true, "wildcardCharacter": true, "ignoreWhiteSpace": true, "caseInsensitiveAndIgnoreWhiteSpace": true, "containNumberBetweenZeroAndNine": true, "containPrefix": true, "containsFiveLetterWordStartingWithS": true}]"####,
    ),
    (
        "replace",
        r####"SELECT VALUE { replaceSubstring: REPLACE("AdventureWorksLT", "LT", "LT2") }"####,
        r####"[{"replaceSubstring": "AdventureWorksLT2"}]"####,
    ),
    ("replicate", r####"SELECT VALUE { catchPhrase: REPLICATE("Cosmic", 3) }"####, r####"[{"catchPhrase": "CosmicCosmicCosmic"}]"####),
    (
        "reverse",
        r####"SELECT VALUE { reverseAdventureWorks: REVERSE("AdventureWorks"), reverseAdventureWorksBack: REVERSE("skroWerutnevdA"), doubleReverseAdventureWorks: REVERSE(REVERSE("AdventureWorks")) }"####,
        r####"[{"reverseAdventureWorks": "skroWerutnevdA", "reverseAdventureWorksBack": "AdventureWorks", "doubleReverseAdventureWorks": "AdventureWorks"}]"####,
    ),
    (
        "right",
        r####"SELECT VALUE { lastZero: RIGHT("AdventureWorks", 0), lastOne: RIGHT("AdventureWorks", 1), lastFive: RIGHT("AdventureWorks", 5), fullLength: RIGHT("AdventureWorks", LENGTH("AdventureWorks")), beyondMaxLength: RIGHT("AdventureWorks", 100) }"####,
        r####"[{"lastZero": "", "lastOne": "s", "lastFive": "Works", "fullLength": "AdventureWorks", "beyondMaxLength": "AdventureWorks"}]"####,
    ),
    (
        "round",
        r####"SELECT VALUE { roundTwoPointFour: ROUND(2.4), roundTwoPointSix: ROUND(2.6), roundTwoPointFive: ROUND(2.5), roundNegativeTwoPointFour: ROUND(-2.4), roundNegativeTwoPointSix: ROUND(-2.6) }"####,
        r####"[{"roundTwoPointFour": 2, "roundTwoPointSix": 3, "roundTwoPointFive": 3, "roundNegativeTwoPointFour": -2, "roundNegativeTwoPointSix": -3}]"####,
    ),
    (
        "round",
        r####"SELECT VALUE { case1: ROUND(1234567, -2), case2: ROUND(1234567, 0), case3: ROUND(1234567, 2), case4: ROUND(1234.567, -2), case5: ROUND(1234.567, 0), case6: ROUND(1234.567, 2) }"####,
        r####"[{"case1": 1234600, "case2": 1234567, "case3": 1234567, "case4": 1200, "case5": 1235, "case6": 1234.57}]"####,
    ),
    (
        "rtrim",
        r####"SELECT VALUE { whitespaceStart: RTRIM("  AdventureWorks"), whitespaceStartEnd: RTRIM("  AdventureWorks  "), whitespaceEnd: RTRIM("AdventureWorks  "), noWhitespace: RTRIM("AdventureWorks"), trimSuffix: RTRIM("AdventureWorks", "Works"), trimPrefix: RTRIM("AdventureWorks", "Adventure"), trimEntireTerm: RTRIM("AdventureWorks", "AdventureWorks"), trimEmptyString: RTRIM("AdventureWorks", "") }"####,
        r####"[{"whitespaceStart": "  AdventureWorks", "whitespaceStartEnd": "  AdventureWorks", "whitespaceEnd": "AdventureWorks", "noWhitespace": "AdventureWorks", "trimSuffix": "Adventure", "trimPrefix": "AdventureWorks", "trimEntireTerm": "", "trimEmptyString": "AdventureWorks"}]"####,
    ),
    (
        "setdifference",
        r####"SELECT VALUE { "case1": SETDIFFERENCE([1, 2, 3], [1, 2, 6, 7]), "case2": SETDIFFERENCE([1, 2, 6, 7], [1, 2, 3]), "case3": SETDIFFERENCE([1, 2, 3, 4], [1, 2, 3, 4, 5, 6]), "case4": SETDIFFERENCE([], [1, 2, 3]), "case5": SETDIFFERENCE([1, 2, 3], []), "case6": SETDIFFERENCE([1, 1, 1, 1], [2, 3, 4]) }"####,
        r####"[{"case1": [3], "case2": [6, 7], "case3": [], "case4": [], "case5": [1, 2, 3], "case6": [1]}]"####,
    ),
    (
        "setequal",
        r####"SELECT VALUE { "case1": SETEQUAL([1, 2, 3], [1, 2, 3]), "case2": SETEQUAL([1, 2, 3], [3, 2, 1]), "case3": SETEQUAL([1, 2, 3, 3], [1, 2, 2, 3, 1, 2]), "case4": SETEQUAL([], [1, 2, 3]), "case5": SETEQUAL([1, true, 'abc'], [true, 1, 'abc']), "case6": SETEQUAL([1, 1, 1, 1], [2, 3, 4]) }"####,
        r####"[{"case1": true, "case2": true, "case3": true, "case4": false, "case5": true, "case6": false}]"####,
    ),
    (
        "setintersect",
        r####"SELECT VALUE { simpleIntersect: SETINTERSECT([1, 2, 3, 4], [3, 4, 5, 6]), emptyIntersect: SETINTERSECT([1, 2, 3, 4], []), duplicatesIntersect: SETINTERSECT([1, 2, 3, 4], [1, 1, 1, 1]), noMatchesIntersect: SETINTERSECT([1, 2, 3, 4], ["A", "B"]), unorderedIntersect: SETINTERSECT([1, 2, "A", "B"], ["A", 1]) }"####,
        r####"[{"simpleIntersect": [3, 4], "emptyIntersect": [], "duplicatesIntersect": [1], "noMatchesIntersect": [], "unorderedIntersect": ["A", 1]}]"####,
    ),
    (
        "setunion",
        r####"SELECT VALUE { simpleUnion: SetUnion([1, 2, 3, 4], [3, 4, 5, 6]), emptyUnion: SetUnion([1, 2, 3, 4], []), duplicatesUnion: SetUnion([1, 2, 3, 4], [1, 1, 1, 1]), unorderedUnion: SetUnion([1, 2, "A", "B"], ["A", 1]) }"####,
        r####"[{"simpleUnion": [1, 2, 3, 4, 5, 6], "emptyUnion": [1, 2, 3, 4], "duplicatesUnion": [1, 2, 3, 4], "unorderedUnion": [1, 2, "A", "B"]}]"####,
    ),
    (
        "sign",
        r####"SELECT VALUE { signNegativeTwo: SIGN(-2), signNegativeOne: SIGN(-1), signZero: SIGN(0), signOne: SIGN(1), signTwo: SIGN(2) }"####,
        r####"[{"signNegativeTwo": -1, "signNegativeOne": -1, "signZero": 0, "signOne": 1, "signTwo": 1}]"####,
    ),
    ("sin", r####"SELECT VALUE { sine: SIN(45.175643) }"####, r####"[{"sine": 0.929607286611012}]"####),
    (
        "sqrt",
        r####"SELECT VALUE { sqrtZero: SQRT(0), sqrtOne: SQRT(1), sqrtFour: SQRT(4), sqrtPrime: SQRT(17), sqrtTwentyFive: SQRT(25) }"####,
        r####"[{"sqrtZero": 0, "sqrtOne": 1, "sqrtFour": 2, "sqrtPrime": 4.123105625617661, "sqrtTwentyFive": 5}]"####,
    ),
    (
        "square",
        r####"SELECT VALUE { squareZero: SQUARE(0), squareOne: SQUARE(1), squareTwo: SQUARE(2), squareThree: SQUARE(3), squareNull: SQUARE(null) }"####,
        r####"[{"squareZero": 0, "squareOne": 1, "squareTwo": 4, "squareThree": 9}]"####,
    ),
    (
        "startswith",
        r####"SELECT VALUE { startsWithWrongPrefix: STARTSWITH("AdventureWorks", "Works"), startsWithCorrectPrefix: STARTSWITH("AdventureWorks", "Adventure"), startsWithPrefixWrongCase: STARTSWITH("AdventureWorks", "adventure"), startsWithPrefixCaseInsensitive: STARTSWITH("AdventureWorks", "adventure", true) }"####,
        r####"[{"startsWithWrongPrefix": false, "startsWithCorrectPrefix": true, "startsWithPrefixWrongCase": false, "startsWithPrefixCaseInsensitive": true}]"####,
    ),
    (
        "stringequals",
        r####"SELECT VALUE { compareSameCase: STRINGEQUALS("AdventureWorks", "AdventureWorks"), compareDifferentCase: STRINGEQUALS("AdventureWorks", "adventureworks"), compareIgnoreCase: STRINGEQUALS("AdventureWorks", "adventureworks", true) }"####,
        r####"[{"compareSameCase": true, "compareDifferentCase": false, "compareIgnoreCase": true}]"####,
    ),
    (
        "stringjoin",
        r####"SELECT VALUE { joinUsingSpaces: STRINGJOIN(["Iropa", "Mountain", "Bike"], " "), joinUsingEmptyString: STRINGJOIN(["Iropa", "Mountain", "Bike"], ""), joinUsingUndefined: STRINGJOIN(["Iropa", "Mountain", "Bike"], undefined), joinUsingCharacter: STRINGJOIN(["6", "7", "4", "3"], "A"), joinUsingPhrase: STRINGJOIN(["Adventure", "LT"], "Works") }"####,
        r####"[{"joinUsingSpaces": "Iropa Mountain Bike", "joinUsingEmptyString": "IropaMountainBike", "joinUsingCharacter": "6A7A4A3", "joinUsingPhrase": "AdventureWorksLT"}]"####,
    ),
    (
        "stringsplit",
        r####"SELECT VALUE { seperateOnLetter: STRINGSPLIT("Handlebar", "e"), seperateOnSymbol: STRINGSPLIT("CARBON_STEEL_BIKE_WHEEL", "_"), seperateOnWhitespace: STRINGSPLIT("Road Bike", " "), seperateOnPhrase: STRINGSPLIT("xenmoun mountain bike", "moun"), undefinedSeperator: STRINGSPLIT("AluminumBikeFrame", undefined), emptySeparatorString: STRINGSPLIT("Helmet", ""), emptySourceString: STRINGSPLIT("", "") }"####,
        r####"[{"seperateOnLetter": ["Handl", "bar"], "seperateOnSymbol": ["CARBON", "STEEL", "BIKE", "WHEEL"], "seperateOnWhitespace": ["Road", "Bike"], "seperateOnPhrase": ["xen", " ", "tain bike"], "emptySeparatorString": ["Helmet"], "emptySourceString": [""]}]"####,
    ),
    (
        "stringtoarray",
        r####"SELECT VALUE { parseEmptyArray: STRINGTOARRAY("[]"), parseArray: STRINGTOARRAY('[ "coats", "gloves", "hats" ]'), complexArray: STRINGTOARRAY('[ { "types": [ "coats", "gloves" ] }, [ "hats" ], 76, false, null ]'), nestedArray: STRINGTOARRAY('[ [ "coats", "gloves" ], [ "hats" ] ]'), invalidArray: STRINGTOARRAY("[ 'coats', 'gloves', 'hats' ]"), parseUndefined: STRINGTOARRAY(undefined), parseNull: STRINGTOARRAY(null) }"####,
        r####"[{"parseEmptyArray": [], "parseArray": ["coats", "gloves", "hats"], "complexArray": [{"types": ["coats", "gloves"]}, ["hats"], 76, false, null], "nestedArray": [["coats", "gloves"], ["hats"]]}]"####,
    ),
    (
        "stringtoboolean",
        r####"SELECT VALUE { parseBooleanString: STRINGTOBOOLEAN("true"), parseWithPrefix: STRINGTOBOOLEAN("true  "), parseWithSuffix: STRINGTOBOOLEAN("  false"), parseWithWhitespace: STRINGTOBOOLEAN("  false  "), parseBoolean: STRINGTOBOOLEAN(true), parseUndefined: STRINGTOBOOLEAN(undefined), parseNull: STRINGTOBOOLEAN(null) }"####,
        r####"[{"parseBooleanString": true, "parseWithPrefix": true, "parseWithSuffix": false, "parseWithWhitespace": false}]"####,
    ),
    (
        "stringtonull",
        r####"SELECT VALUE { parseNullString: STRINGTONULL("null"), parseWithPrefix: STRINGTONULL("null  "), parseWithSuffix: STRINGTONULL("  null"), parseWithWhitespace: STRINGTONULL("  null  "), parseUppercase: STRINGTONULL("NULL"), parseTitlecase: STRINGTONULL("Null"), parseNull: STRINGTONULL(null), parseUndefined: STRINGTONULL(undefined) }"####,
        r####"[{"parseNullString": null, "parseWithPrefix": null, "parseWithSuffix": null, "parseWithWhitespace": null}]"####,
    ),
    (
        "stringtonumber",
        r####"SELECT VALUE { parseIntegerString: STRINGTONUMBER("100"), parseDecimalString: STRINGTONUMBER("3.14"), parseWithWhitespace: STRINGTONUMBER("   60   "), parseScientific: STRINGTONUMBER("-1.79769e+308"), parseInvalid: STRINGTONUMBER("Hello"), parseUndefined: STRINGTONUMBER(undefined), parseNull: STRINGTONUMBER(null), parseNaN: STRINGTONUMBER(NaN), parseInfinity: STRINGTONUMBER(Infinity) }"####,
        r####"[{"parseIntegerString": 100, "parseDecimalString": 3.14, "parseWithWhitespace": 60, "parseScientific": -1.79769e+308}]"####,
    ),
    (
        "stringtoobject",
        r####"SELECT VALUE { parseEmptyObject: STRINGTOOBJECT("{}"), parseObjectWithProperty: STRINGTOOBJECT('{"isAvailable": true}'), parseObjectNested: STRINGTOOBJECT('{"division": {"name": "Sales"}}'), parseObjectInvalidJson: STRINGTOOBJECT("{'price': 27.55}"), parseUndefined: STRINGTONUMBER(undefined), parseNull: STRINGTONUMBER(null) }"####,
        r####"[{"parseEmptyObject": {}, "parseObjectWithProperty": {"isAvailable": true}, "parseObjectNested": {"division": {"name": "Sales"}}}]"####,
    ),
    (
        "substring",
        r####"SELECT VALUE { substringPrefix: SUBSTRING("AdventureWorks", 0, 9), substringSuffix: SUBSTRING("AdventureWorks", 9, 5), substringTotalLength: SUBSTRING("AdventureWorks", 0, LENGTH("AdventureWorks")), substringEmptyString: SUBSTRING("AdventureWorks", 0, -1) }"####,
        r####"[{"substringPrefix": "Adventure", "substringSuffix": "Works", "substringTotalLength": "AdventureWorks", "substringEmptyString": ""}]"####,
    ),
    (
        "tan",
        r####"SELECT VALUE { tangentSquareRootPi: TAN(PI()/2), tangentArbitraryNumber: TAN(124.1332) }"####,
        r####"[{"tangentSquareRootPi": 16331239353195370, "tangentArbitraryNumber": -24.80651023035602}]"####,
    ),
    (
        "tickstodatetime",
        r####"SELECT VALUE { parseTicks: TICKSTODATETIME(15973607943002652), parseUnixEpoch: TICKSTODATETIME(0), parseWindowsEpoch: TICKSTODATETIME(-116444736000000000) }"####,
        r####"[{"parseTicks": "2020-08-13T23:19:54.3002652Z", "parseUnixEpoch": "1970-01-01T00:00:00.0000000Z", "parseWindowsEpoch": "1601-01-01T00:00:00.0000000Z"}]"####,
    ),
    (
        "timestamptodatetime",
        r####"SELECT VALUE { parseTicks: TIMESTAMPTODATETIME(1597360794300), parseUnixEpoch: TIMESTAMPTODATETIME(0), parseWindowsEpoch: TIMESTAMPTODATETIME(-11644473600000) }"####,
        r####"[{"parseTicks": "2020-08-13T23:19:54.3000000Z", "parseUnixEpoch": "1970-01-01T00:00:00.0000000Z", "parseWindowsEpoch": "1601-01-01T00:00:00.0000000Z"}]"####,
    ),
    (
        "tostring",
        r####"SELECT VALUE { integerToString: TOSTRING(125), floatToString: TOSTRING(0.1234), booleanToString: TOSTRING(false), arrayToString: TOSTRING([ 1, 2, 3 ]), objectToString: TOSTRING({ "department": "Bicycles" }), stringToString: TOSTRING("Hello World"), undefinedToString: TOSTRING(undefined), notANumberToString: TOSTRING(NaN), infinityToString: TOSTRING(Infinity) }"####,
        r####"[{"integerToString": "125", "floatToString": "0.1234", "booleanToString": "false", "arrayToString": "[1,2,3]", "objectToString": "{\"department\":\"Bicycles\"}", "stringToString": "Hello World", "notANumberToString": "NaN", "infinityToString": "Infinity"}]"####,
    ),
    (
        "trim",
        r####"SELECT VALUE { trimPrefix: TRIM("   AdventureWorks"), trimSuffix: TRIM("AdventureWorks   "), trimWhitespace: TRIM("   AdventureWorks   "), trimWrongCharacter: TRIM("---AdventureWorks---"), trimUnderscores: TRIM("___AdventureWorks___", "_"), trimHyphens: TRIM("---AdventureWorks---", "-"), trimSubsetCharacters: TRIM("-- AdventureWorks --", "-"), trimMultipleCharacters: TRIM("-_-AdventureWorks-_-", "-_") }"####,
        r####"[{"trimPrefix": "AdventureWorks", "trimSuffix": "AdventureWorks", "trimWhitespace": "AdventureWorks", "trimWrongCharacter": "---AdventureWorks---", "trimUnderscores": "AdventureWorks", "trimHyphens": "AdventureWorks", "trimSubsetCharacters": " AdventureWorks ", "trimMultipleCharacters": "AdventureWorks"}]"####,
    ),
    (
        "trunc",
        r####"SELECT VALUE { truncateFloatingPoint: TRUNC(2.37), truncateNegative: TRUNC(-2.78), truncateInteger: TRUNC(2), truncateSmallNumber: TRUNC(0.0000714), truncatePi: TRUNC(PI()) }"####,
        r####"[{"truncateFloatingPoint": 2, "truncateNegative": -2, "truncateInteger": 2, "truncateSmallNumber": 0, "truncatePi": 3}]"####,
    ),
    (
        "upper",
        r####"SELECT VALUE { lowercase: UPPER("adventureworks"), uppercase: UPPER("ADVENTUREWORKS"), camelCase: UPPER("adventureWorks"), pascalCase: UPPER("AdventureWorks"), upperSnakeCase: UPPER("ADVENTURE_WORKS") }"####,
        r####"[{"lowercase": "ADVENTUREWORKS", "uppercase": "ADVENTUREWORKS", "camelCase": "ADVENTUREWORKS", "pascalCase": "ADVENTUREWORKS", "upperSnakeCase": "ADVENTURE_WORKS"}]"####,
    ),
    (
        "year",
        r####"SELECT VALUE { "case1": YEAR("2000-10-10"), "case2": YEAR("2024-01-10T14:15:20"), "case3": YEAR("1989-03-03T12:12:12.1234567Z") }"####,
        r####"[{"case1": 2000, "case2": 2024, "case3": 1989}]"####,
    ),
];

#[test]
fn doc_examples() {
    let mut failures = Vec::new();
    let opts = fixed_clock_opts();
    for (page, sql, expected) in EXAMPLES {
        let expected: Value = serde_json::from_str(expected).unwrap();
        match try_run(sql, &[], &opts) {
            Ok(got) => {
                if !approx_eq(&Value::Array(got.clone()), &expected) {
                    failures.push(format!("[{page}] {sql}\n   expected {expected}\n   got      {}", Value::Array(got)));
                }
            }
            Err(e) => failures.push(format!("[{page}] {sql}\n   error {e}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} doc examples failed:\n{}", failures.len(), EXAMPLES.len(), failures.join("\n"));
}
