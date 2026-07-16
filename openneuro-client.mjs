const OPENNEURO_GRAPHQL = 'https://openneuro.org/crn/graphql';

const POPULAR_DATASETS_QUERY = `
  query PopularDatasets($first: Int!) {
    datasets(
      first: $first
      filterBy: { public: true }
      modality: "MRI"
      orderBy: { downloads: descending }
    ) {
      edges {
        node {
          id
          name
          publishDate
          metadata {
            datasetName
            modalities
            tasksCompleted
            studyDomain
            species
          }
          latestSnapshot { tag size created }
        }
      }
    }
  }
`;

const DATASET_QUERY = `
  query PreviewDataset($id: ID!) {
    dataset(id: $id) {
      id
      name
      public
      metadata {
        datasetName
        modalities
        tasksCompleted
        studyDomain
        species
        openneuroPaperDOI
        associatedPaperDOI
      }
      latestSnapshot {
        tag
        size
        created
        description {
          Name
          DatasetDOI
          License
          Authors
          HowToAcknowledge
        }
        files(recursive: true) {
          id
          filename
          size
          directory
          urls
        }
      }
    }
  }
`;

async function graphql(query, variables, options = {}) {
  const fetchImpl = options.fetchImpl ?? fetch;
  const response = await fetchImpl(OPENNEURO_GRAPHQL, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query, variables }),
    cache: 'no-store',
    signal: options.signal,
  });
  if (!response.ok) throw new Error(`OpenNeuro returned ${response.status}`);
  const result = await response.json();
  if (result.errors?.length) {
    throw new Error(result.errors[0]?.message || 'OpenNeuro returned a GraphQL error');
  }
  return result.data;
}

export function formatBytes(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return '—';
  if (value < 1024) return `${value} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let scaled = value;
  let unit = -1;
  do {
    scaled /= 1024;
    unit++;
  } while (scaled >= 1024 && unit < units.length - 1);
  const digits = scaled >= 100 ? 0 : scaled >= 10 ? 1 : 2;
  return `${scaled.toFixed(digits)} ${units[unit]}`;
}

export function parseBidsPath(path) {
  const filename = path.split('/').pop() || path;
  const stem = filename.replace(/\.nii(?:\.gz)?$/i, '');
  const parts = stem.split('_');
  const suffix = parts.pop() || 'image';
  const entities = {};
  parts.forEach((part) => {
    const separator = part.indexOf('-');
    if (separator > 0) entities[part.slice(0, separator)] = part.slice(separator + 1);
  });
  return { filename, suffix, entities };
}

function inferModality(path, suffix) {
  const lower = suffix.toLowerCase();
  if (lower === 'bold') return 'bold';
  if (lower === 'dwi' || path.includes('/dwi/')) return 'dwi';
  if (path.includes('/anat/') || ['t1w', 't2w', 'flair', 'inplanet1', 'inplanet2'].includes(lower)) return 'anat';
  if (path.includes('/fmap/') || ['fieldmap', 'phasediff', 'magnitude1', 'magnitude2', 'epi'].includes(lower)) return 'fmap';
  return 'other';
}

function previewSort(a, b) {
  const priority = { bold: 0, anat: 1, dwi: 2, fmap: 3, other: 4 };
  return priority[a.mod] - priority[b.mod] || a.path.localeCompare(b.path, undefined, { numeric: true });
}

export function filesToPreviewScans(files, dataset) {
  return (files || [])
    .filter((file) => (
      !file.directory &&
      !file.filename.startsWith('derivatives/') &&
      /\.nii(?:\.gz)?$/i.test(file.filename) &&
      file.urls?.some((url) => /^https:\/\//.test(url))
    ))
    .map((file) => {
      const parsed = parseBidsPath(file.filename);
      const mod = inferModality(file.filename, parsed.suffix);
      const subject = parsed.entities.sub ? `sub-${parsed.entities.sub}` : 'dataset';
      const session = parsed.entities.ses ? `ses-${parsed.entities.ses}` : 'single session';
      const task = parsed.entities.task ? `task-${parsed.entities.task}` : null;
      return {
        id: `${dataset.id}:${file.filename}`,
        datasetId: dataset.id,
        datasetName: dataset.name,
        snapshot: dataset.snapshot,
        source: file.urls.find((url) => /^https:\/\//.test(url)),
        path: file.filename,
        filename: parsed.filename,
        pid: subject,
        ses: session,
        task,
        suffix: parsed.suffix,
        mod,
        title: task ? `${task} · ${parsed.suffix}` : parsed.suffix,
        sizeBytes: Number(file.size || 0),
        size: formatBytes(file.size),
        openNeuroUrl: `https://openneuro.org/datasets/${dataset.id}/versions/${dataset.snapshot}`,
        realNifti: true,
      };
    })
    .sort(previewSort)
    .map((scan, index) => ({ ...scan, idx: index }));
}

function normalizeDataset(node) {
  if (!node) return null;
  const name = node.metadata?.datasetName || node.name || node.id;
  return {
    id: node.id,
    name,
    tasks: node.metadata?.tasksCompleted?.filter(Boolean) || [],
    domain: node.metadata?.studyDomain || '',
    modalities: node.metadata?.modalities || [],
    snapshot: node.latestSnapshot?.tag || '',
    sizeBytes: Number(node.latestSnapshot?.size || 0),
    size: formatBytes(node.latestSnapshot?.size),
    updated: node.latestSnapshot?.created || node.publishDate || '',
  };
}

export function datasetMatches(dataset, query) {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return [dataset.id, dataset.name, dataset.domain, ...dataset.tasks]
    .join(' ')
    .toLowerCase()
    .includes(needle);
}

export async function fetchPopularOpenNeuroDatasets(options = {}) {
  const data = await graphql(POPULAR_DATASETS_QUERY, { first: options.first ?? 40 }, options);
  return (data.datasets?.edges || []).map((edge) => normalizeDataset(edge.node)).filter(Boolean);
}

export async function fetchOpenNeuroDataset(id, options = {}) {
  const normalizedId = String(id || '').trim().toLowerCase();
  if (!/^ds\d{6}$/.test(normalizedId)) throw new Error('Enter an OpenNeuro accession such as ds000001');
  const data = await graphql(DATASET_QUERY, { id: normalizedId }, options);
  if (!data.dataset?.public) throw new Error(`${normalizedId} is not a public OpenNeuro dataset`);

  const node = data.dataset;
  const snapshot = node.latestSnapshot;
  if (!snapshot?.tag) throw new Error(`${normalizedId} has no published OpenNeuro snapshot`);
  const dataset = {
    ...normalizeDataset(node),
    snapshot: snapshot.tag,
    doi: snapshot.description?.DatasetDOI || node.metadata?.openneuroPaperDOI || '',
    license: snapshot.description?.License || '',
    authors: snapshot.description?.Authors || [],
    howToAcknowledge: snapshot.description?.HowToAcknowledge || '',
  };
  dataset.scans = filesToPreviewScans(snapshot.files, dataset);
  if (!dataset.scans.length) throw new Error(`${normalizedId} has no directly previewable NIfTI files`);
  return dataset;
}
