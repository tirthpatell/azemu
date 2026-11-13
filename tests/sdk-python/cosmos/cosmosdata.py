"""Sample documents shared by the query tests.

PRODUCTS (container partitioned on /category): 24 documents, 6 per category,
with a mix of ints and floats, null / missing properties, arrays, nested
objects and arrays of objects. Hand written so that expected query results can
be read straight off the table.

FAMILIES (container partitioned on /lastName): 6 documents, shaped like the
"families" sample in the Cosmos DB query documentation (nested children, pets).
"""


def _p(id, category, name, price, stock, tags, **extra):
    doc = {
        "id": id,
        "category": category,
        "name": name,
        "price": price,
        "stock": stock,
        "tags": tags,
    }
    doc.update(extra)
    return doc


def _r(user, stars):
    return {"user": user, "stars": stars}


PRODUCTS = [
    # ---- books -------------------------------------------------------------
    _p("p01", "books", "Rust in Action", 39.99, 12, ["programming", "rust"],
       rating=4.8, brand="Manning", discontinued=False,
       reviews=[_r("ann", 5), _r("bob", 4)]),
    _p("p02", "books", "Python Cookbook", 45, 0, ["programming", "python"],
       rating=4.5, brand="OReilly", discontinued=False, reviews=[_r("cy", 5)]),
    _p("p03", "books", "Cooking for Two", 22.5, 30, ["cooking"],
       rating=None, discontinued=False, reviews=[]),
    _p("p04", "books", "Old Atlas", 15, 3, ["maps", "history"],
       brand="Penguin", discontinued=True, reviews=[_r("ann", 2)]),
    _p("p05", "books", "Poetry Collection", 9.5, 100, [],
       rating=3.9, brand="Penguin", discontinued=False),
    _p("p06", "books", "SQL Handbook", 55, 7, ["programming", "databases", "sql"],
       rating=4.2, brand="OReilly", discontinued=False,
       reviews=[_r("bob", 3), _r("dee", 4)]),
    # ---- electronics -------------------------------------------------------
    _p("p07", "electronics", "Laptop Pro", 1299.99, 5, ["computer", "portable"],
       rating=4.7, brand="Acme", discontinued=False,
       specs={"cpu": "x86", "ram_gb": 16, "dims": {"w": 30, "h": 2}},
       reviews=[_r("ann", 5), _r("eve", 4), _r("bob", 5)]),
    _p("p08", "electronics", "Phone X", 799, 20, ["phone", "portable"],
       rating=4.1, brand="Acme", discontinued=False,
       specs={"ram_gb": 8, "dims": {"w": 7, "h": 15}}, reviews=[_r("cy", 4)]),
    _p("p09", "electronics", "Headphones", 59.5, 45, ["audio", "portable"],
       rating=4.0, brand="Sonic", discontinued=False,
       specs={"wireless": True}, reviews=[]),
    _p("p10", "electronics", "Old Radio", 25, 2, ["audio"],
       rating=3.0, brand="Sonic", discontinued=True),
    _p("p11", "electronics", "Smart Watch", 199, 0, ["wearable", "portable"],
       rating=None, brand="Acme", discontinued=False, specs={"ram_gb": 1}),
    _p("p12", "electronics", "USB Cable", 4.99, 500, [],
       rating=3.5, discontinued=False),
    # ---- garden ------------------------------------------------------------
    _p("p13", "garden", "Garden Hose", 24.5, 60, ["watering", "outdoor"],
       rating=4.2, brand="GreenCo", discontinued=False),
    _p("p14", "garden", "Shovel", 18, 40, ["tools", "outdoor"],
       rating=4.4, brand="GreenCo", discontinued=False),
    _p("p15", "garden", "Rose Seeds", 3.25, 300, ["seeds", "flowers"],
       brand="Bloom", discontinued=False),
    _p("p16", "garden", "Lawn Mower", 249, 4, ["tools", "outdoor", "motor"],
       rating=4.6, brand="GreenCo", discontinued=False),
    _p("p17", "garden", "Plant Pot", 12, 80, ["pots"],
       rating=3.8, brand="Bloom", discontinued=True),
    _p("p18", "garden", "Bird Feeder", 17.75, 15, ["outdoor", "birds"],
       rating=4.9, discontinued=False),
    # ---- toys --------------------------------------------------------------
    _p("p19", "toys", "Toy Train", 35, 10, ["trains", "wooden"],
       rating=4.3, brand="Playful", discontinued=False),
    _p("p20", "toys", "Puzzle 1000", 14.99, 25, ["puzzle", "indoor"],
       rating=4.0, brand="Playful", discontinued=False),
    _p("p21", "toys", "Robot Kit", 89.5, 6, ["robot", "programming", "stem"],
       rating=4.8, brand="Acme", discontinued=False),
    _p("p22", "toys", "Building Blocks", 29, 0, ["blocks", "indoor"],
       rating=4.1, brand="Playful", discontinued=False),
    _p("p23", "toys", "Kite", 12.5, 33, ["outdoor", "wind"],
       rating=3.9, brand="Windy", discontinued=False),
    _p("p24", "toys", "Teddy Bear", 19.99, 50, ["plush", "indoor"],
       rating=4.6, brand="Playful", discontinued=False),
]

CATEGORIES = ["books", "electronics", "garden", "toys"]


FAMILIES = [
    {
        "id": "AndersenFamily",
        "lastName": "Andersen",
        "parents": [{"firstName": "Thomas"}, {"firstName": "Mary Kay"}],
        "children": [
            {"firstName": "Henriette Thaulow", "gender": "female", "grade": 5,
             "pets": [{"givenName": "Fluffy"}]},
        ],
        "address": {"state": "WA", "county": "King", "city": "Seattle"},
        "creationDate": 1431620472,
        "isRegistered": True,
    },
    {
        "id": "WakefieldFamily",
        "lastName": "Wakefield",
        "parents": [
            {"familyName": "Wakefield", "givenName": "Robin"},
            {"familyName": "Miller", "givenName": "Ben"},
        ],
        "children": [
            {"familyName": "Merriam", "givenName": "Jesse", "gender": "female", "grade": 1,
             "pets": [{"givenName": "Goofy"}, {"givenName": "Shadow"}]},
            {"familyName": "Miller", "givenName": "Lisa", "gender": "female", "grade": 8},
        ],
        "address": {"state": "NY", "county": "Manhattan", "city": "NY"},
        "creationDate": 1431620462,
        "isRegistered": False,
    },
    {
        "id": "SmithFamily",
        "lastName": "Smith",
        "parents": [{"firstName": "Carl"}, {"firstName": "Dana"}],
        "children": [
            {"firstName": "Anna", "gender": "female", "grade": 3, "pets": []},
            {"firstName": "Ben", "gender": "male", "grade": 5, "pets": [{"givenName": "Rex"}]},
        ],
        "address": {"state": "WA", "city": "Tacoma"},
        "creationDate": 1431620500,
        "isRegistered": True,
    },
    {
        "id": "JonesFamily",
        "lastName": "Jones",
        "parents": [{"firstName": "Pat"}],
        "children": [],
        "address": {"state": "CA", "city": "LA"},
        "creationDate": 1431620520,
        "isRegistered": False,
    },
    {
        "id": "AndersenFamily2",
        "lastName": "Andersen",
        "parents": [{"firstName": "Lars"}],
        "children": [
            {"firstName": "Olaf", "gender": "male", "grade": 2, "pets": [{"givenName": "Sven"}]},
        ],
        "address": {"state": "OR", "city": "Portland"},
        "creationDate": 1431620530,
        "isRegistered": True,
    },
    {
        "id": "NoAddressFamily",
        "lastName": "Smith",
        "parents": [{"firstName": "Quinn"}],
        "children": [{"firstName": "Zed", "gender": "male", "grade": 7}],
        "creationDate": 1431620540,
    },
]
