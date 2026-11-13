// Quick query (Query Blob Contents) with @azure/storage-blob: BlockBlobClient.query.
// Expectations come from the SDK's own live tests (blockblobclient query tests) and the REST page "Query Blob Contents".
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { BlobServiceClient, StorageSharedKeyCredential } from '@azure/storage-blob';
import { env, uniq, DEV_ACCOUNT, DEV_KEY, streamToBuffer, rejection } from '../lib/emu.mjs';

const emu = env();
const svc = new BlobServiceClient(emu.url('blob', DEV_ACCOUNT), new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY));

const CSV = 'id,name,price\r\n1,apple,0.5\r\n2,banana,0.25\r\n3,"cherry, red",4\r\n';
const LINES = CSV.replaceAll('\r\n', '\n');

async function blob(data, prefix = 'q') {
  const c = svc.getContainerClient(uniq('c'));
  await c.create();
  const b = c.getBlockBlobClient(uniq(prefix));
  await b.upload(data, Buffer.byteLength(data));
  return b;
}
async function run(b, query, options = {}) {
  const r = await b.query(query, options);
  return (await streamToBuffer(r.readableStreamBody)).toString();
}
const csvIn = { kind: 'csv', recordSeparator: '\n', hasHeaders: true };

describe('query blob contents (JavaScript SDK)', () => {
  it('select * returns the blob with \\n line ends and the avro headers', async () => {
    const b = await blob(CSV);
    const r = await b.query('select * from BlobStorage');
    assert.equal(r._response.status, 200);
    assert.equal(r.contentType, 'avro/binary');
    assert.ok(r.etag && r.lastModified);
    assert.equal((await streamToBuffer(r.readableStreamBody)).toString(), LINES);
  });

  it('csv in and out with headers, a where clause and a projection', async () => {
    const b = await blob(CSV);
    const out = await run(b, 'SELECT name, price * 2 AS double FROM BlobStorage WHERE id > 1', {
      inputTextConfiguration: csvIn,
      outputTextConfiguration: { kind: 'csv', recordSeparator: '\n' },
    });
    assert.equal(out, 'banana,0.5\n"cherry, red",8\n');
  });

  it('csv output with a header row and custom separators', async () => {
    const b = await blob(CSV);
    const out = await run(b, 'SELECT id, name FROM BlobStorage', {
      inputTextConfiguration: csvIn,
      outputTextConfiguration: { kind: 'csv', recordSeparator: '|', columnSeparator: ';', fieldQuote: "'", escapeCharacter: '\\', hasHeaders: true },
    });
    // the quote is `'`, and the field that holds a `,` needs no quotes here because the column separator is `;`
    assert.equal(out, 'id;name|1;apple|2;banana|3;cherry, red|');
  });

  it('json in and json out, missing fields are left out', async () => {
    const b = await blob('{"name": "owner", "id": 1}\n{"name2": "owner2"}\n{"name": "owner", "id": 1}');
    const out = await run(b, 'SELECT name FROM BlobStorage', {
      inputTextConfiguration: { kind: 'json', recordSeparator: '\n' },
      outputTextConfiguration: { kind: 'json', recordSeparator: ';' },
    });
    assert.equal(out, '{"name":"owner"};{};{"name":"owner"};');
  });

  it('json paths, arrays and IS MISSING', async () => {
    const doc = { id: 1, tags: ['a', 'b'], dimensions: { length: 3 }, warehouses: [{ latitude: 41.8 }, { latitude: 1.5 }] };
    const b = await blob(JSON.stringify(doc));
    const cfg = { inputTextConfiguration: { kind: 'json', recordSeparator: '\n' }, outputTextConfiguration: { kind: 'json', recordSeparator: '\n' } };
    assert.equal(await run(b, 'SELECT latitude FROM BlobStorage[*].warehouses[*]', cfg), '{"latitude":41.8}\n{"latitude":1.5}\n');
    assert.equal(await run(b, 'SELECT length FROM BlobStorage[*].dimensions', cfg), '{"length":3}\n');
    assert.equal(await run(b, 'SELECT id FROM BlobStorage WHERE weight IS MISSING', cfg), '{"id":1}\n');
  });

  it('aggregates', async () => {
    const b = await blob('1\n2\n3\n4\n');
    assert.equal(await run(b, 'SELECT COUNT(*), SUM(CAST(_1 AS INT)), AVG(CAST(_1 AS INT)) FROM BlobStorage'), '4,10,2.5\n');
  });

  it('onProgress sees the scanned bytes and onError sees a fatal parse error', async () => {
    const b = await blob(CSV);
    const progress = [];
    await run(b, 'SELECT * FROM BlobStorage', { onProgress: (p) => progress.push(p.loadedBytes) });
    assert.ok(progress.length >= 2);
    assert.equal(progress.at(-1), CSV.length);

    const bad = await blob('{name: owner}\n');
    const errors = [];
    const out = await run(bad, 'SELECT * FROM BlobStorage', {
      inputTextConfiguration: { kind: 'json', recordSeparator: '\n' },
      outputTextConfiguration: { kind: 'csv', recordSeparator: '\n' },
      onError: (e) => errors.push(e),
    });
    assert.equal(out, '');
    assert.equal(errors.length, 1);
    assert.deepEqual(errors[0], { position: 0, name: 'ParseError', isFatal: true, description: "Unexpected token 'n' at [byte: 1]. Expecting token '\"'. " });
  });

  it('a row with a missing column is a non-fatal error', async () => {
    const b = await blob(CSV + 'lonely\r\n');
    const errors = [];
    const out = await run(b, 'SELECT price FROM BlobStorage', { inputTextConfiguration: csvIn, outputTextConfiguration: { kind: 'csv', recordSeparator: '\n' }, onError: (e) => errors.push(e) });
    assert.equal(out, '0.5\n0.25\n4\n');
    assert.deepEqual(errors.map((e) => [e.name, e.isFatal, e.position]), [['InvalidColumnOrdinal', false, CSV.length]]);
  });

  it('a bad expression is a fatal error in the stream', async () => {
    const b = await blob(CSV);
    const errors = [];
    assert.equal(await run(b, 'SELEKT * FROM BlobStorage', { onError: (e) => errors.push(e) }), '');
    assert.deepEqual(errors.map((e) => [e.name, e.isFatal]), [['ParseError', true]]);
  });

  it('a missing blob, an etag condition and an append blob', async () => {
    const b = await blob(CSV);
    const gone = svc.getContainerClient(b.containerName).getBlockBlobClient('nope');
    const e = await rejection(() => gone.query('SELECT * FROM BlobStorage'));
    assert.equal(e.statusCode, 404);
    const e2 = await rejection(() => b.query('SELECT * FROM BlobStorage', { conditions: { ifMatch: '"0x1"' } }));
    assert.equal(e2.statusCode, 412);
    const app = svc.getContainerClient(b.containerName).getAppendBlobClient(uniq('app'));
    await app.create();
    const e3 = await rejection(() => svc.getContainerClient(b.containerName).getBlockBlobClient(app.name).query('SELECT * FROM BlobStorage'));
    assert.equal(e3.statusCode, 409);
  });

  it('a snapshot is queried as it was', async () => {
    const b = await blob('old\n');
    const snap = await b.createSnapshot();
    await b.upload('new\n', 4);
    assert.equal(await run(b, 'SELECT * FROM BlobStorage'), 'new\n');
    assert.equal(await run(b.withSnapshot(snap.snapshot), 'SELECT * FROM BlobStorage'), 'old\n');
  });

  it('a result of several megabytes arrives whole', async () => {
    const rows = Array.from({ length: 200000 }, (_, i) => `${i},value-${i}`).join('\n') + '\n';
    const b = await blob(rows);
    const out = await run(b, 'SELECT * FROM BlobStorage');
    assert.equal(out.length, rows.length);
    assert.equal(out, rows);
  });

  it('parquet input and arrow output are refused with a 501', async () => {
    const b = await blob(CSV);
    const e = await rejection(() => b.query('select * from blobstorage', { inputTextConfiguration: { kind: 'parquet' } }));
    assert.equal(e.statusCode, 501);
    const e2 = await rejection(() => b.query('SELECT _2 FROM BlobStorage', { outputTextConfiguration: { kind: 'arrow', schema: [{ type: 'decimal', name: 'abc', precision: 4, scale: 2 }] } }));
    assert.equal(e2.statusCode, 501);
  });
});
