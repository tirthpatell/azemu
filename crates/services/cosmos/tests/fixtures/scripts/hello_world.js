// Source: https://learn.microsoft.com/en-us/azure/cosmos-db/nosql/how-to-write-stored-procedures-triggers-udfs ("How to write stored procedures", the serverScript of helloWorldStoredProc)
function () {
        var context = getContext();
        var response = context.getResponse();

        response.setBody("Hello, World");
    }
