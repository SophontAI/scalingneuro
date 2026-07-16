import assert from 'node:assert/strict';
import test from 'node:test';

import {
  datasetMatches,
  fetchOpenNeuroDataset,
  filesToPreviewScans,
  formatBytes,
  parseBidsPath,
} from '../openneuro-client.mjs';

test('parses BIDS entities and suffixes from a nested path', () => {
  assert.deepEqual(
    parseBidsPath('sub-01/ses-02/func/sub-01_ses-02_task-rest_run-03_bold.nii.gz'),
    {
      filename: 'sub-01_ses-02_task-rest_run-03_bold.nii.gz',
      suffix: 'bold',
      entities: { sub: '01', ses: '02', task: 'rest', run: '03' },
    },
  );
});

test('creates sorted raw preview records and excludes derivatives', () => {
  const scans = filesToPreviewScans([
    { filename: 'sub-01/anat/sub-01_T1w.nii.gz', size: 1024, directory: false, urls: ['https://s3/a'] },
    { filename: 'sub-01/func/sub-01_task-rest_bold.nii.gz', size: 2048, directory: false, urls: ['https://s3/b'] },
    { filename: 'derivatives/sub-01_task-rest_bold.nii.gz', size: 1, directory: false, urls: ['https://s3/c'] },
  ], { id: 'ds000001', name: 'Example', snapshot: '1.0.0' });

  assert.equal(scans.length, 2);
  assert.equal(scans[0].mod, 'bold');
  assert.equal(scans[0].pid, 'sub-01');
  assert.equal(scans[0].task, 'task-rest');
  assert.equal(scans[1].mod, 'anat');
});

test('matches datasets across accession, title, task, and domain', () => {
  const dataset = { id: 'ds002748', name: 'Depression resting state', tasks: ['rest'], domain: 'fMRI' };
  assert.equal(datasetMatches(dataset, '2748'), true);
  assert.equal(datasetMatches(dataset, 'rest'), true);
  assert.equal(datasetMatches(dataset, 'fmri'), true);
  assert.equal(datasetMatches(dataset, 'movie'), false);
});

test('formats byte sizes for compact archive labels', () => {
  assert.equal(formatBytes(1024), '1.00 KB');
  assert.equal(formatBytes(5 * 1024 ** 3), '5.00 GB');
});

test('loads a public dataset and retains version-pinned file URLs', async () => {
  const payload = {
    data: {
      dataset: {
        id: 'ds000001',
        name: 'Example',
        public: true,
        metadata: { datasetName: 'Example dataset', tasksCompleted: ['rest'], modalities: ['mri'] },
        latestSnapshot: {
          tag: '1.2.3',
          size: 2048,
          created: '2026-01-01T00:00:00Z',
          description: { DatasetDOI: '10.18112/openneuro.ds000001.v1.2.3', License: 'CC0' },
          files: [{
            filename: 'sub-01/func/sub-01_task-rest_bold.nii.gz',
            size: 2048,
            directory: false,
            urls: ['https://s3.amazonaws.com/openneuro.org/file?versionId=abc'],
          }],
        },
      },
    },
  };
  const dataset = await fetchOpenNeuroDataset('DS000001', {
    fetchImpl: async () => new Response(JSON.stringify(payload), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }),
  });

  assert.equal(dataset.id, 'ds000001');
  assert.equal(dataset.snapshot, '1.2.3');
  assert.equal(dataset.scans[0].source.endsWith('versionId=abc'), true);
});
