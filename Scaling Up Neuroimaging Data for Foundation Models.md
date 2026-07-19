# **Scaling Up Neuroimaging Data for Foundation Models**

Modern AI models routinely learn from billions of examples. For language, text scraped from the web enabled foundation models like GPT; for vision, large image datasets like ImageNet were critical. By contrast, neuroimaging data are scarce, fragmented and often locked behind restrictive agreements. Large, well‐curated collections exist (e.g., Human Connectome Project, UK Biobank and OpenNeuro), but their combined hours of fMRI data are tiny compared with web‑scale datasets and access to them is often gated by lengthy applications and usage restrictions. Scaling dataset size is arguably the most critical barrier currently blocking the full potential of neuroAI.

We propose a community‑led effort to build the largest open neuroimaging dataset, spanning functional, diffusion and structural MRI. The goal is to collect tens of thousands of hours of recordings and structural scans each year by automatically sharing unlabeled, privacy-cleared data from MRI facilities. This document outlines the technical plan, legal considerations, funding opportunities, and outreach strategy.

## **Proposed automatic data‑sharing pipeline**

### **Workflow overview**

1. **Automatic data capture** – In the first production route, a researcher runs one terminal command against a completed DICOM export folder. The local tool identifies confidently supported EPI/fMRI series and prepares them for transfer to secure cloud storage. Structural, diffusion, ASL, field-map, reference, derived, and uncertain series remain local. A later unattended watcher can use the same deterministic folder/series identities without changing the archive contract.

2. **Modality-specific privacy** – The initial EPI-only route does not upload structural scans and therefore does not pretend to solve defacing. It recursively de-identifies DICOM headers, preserves scanner-native Pixel Data exactly, and fails closed on burned-in annotation or unsupported metadata. Structural MRI will use a separate future route with validated defacing and quantitative brain-preservation QC before transfer.

3. **Deidentification without scientific metadata loss** – Names, medical record numbers, dates, accessions, administrative text, source UIDs, and unsafe private fields are removed or pseudonymized locally. Standard geometry, pixel semantics, scanner manufacturer/model/software, field strength, coils, sequence encodings, TR/TE, acceleration, phase-encoding, and other reviewed acquisition metadata are retained. Vendor-private fields are default-deny and enabled only through fixture-backed, bounded reconstruction rules.

4. **Unlabeled, research-raw data sharing** – Because the dataset is intended for self-supervised foundation models, no manual labeling, BIDS naming, task annotation, or local curation is required. The canonical archive retains privacy-cleared scanner-native DICOM pixels, acquisition metadata, a deterministic instance inventory, and explicit privacy/classification provenance. Governed contributor access to the aggregated archive is a separate product surface rather than a credential distributed by the ingestion command.

5. **Data format and storage** – Source DICOMs remain unchanged at the institution. Cloudflare R2 stores one immutable, deterministic, privacy-cleared DICOM archive per accepted series, with complete hashes and provenance. Sophont asynchronously verifies those exact bytes, repeats the privacy and EPI-purpose audit, runs a pinned converter, and publishes deterministic NIfTI, minimized sidecar, and processing-manifest derivatives. Derived representations and training caches never replace the canonical source archive.

6. **Compute integration** – Training jobs can pull data directly from the cloud storage.

### **Implementation details**

* **Portable deployment** – The production experience is a readable terminal installer followed by `neuro-sync /path/to/dicoms`, with no browser, administrator access, manual Python, FSL, Docker, converter, cloud CLI, or GPU setup. The same terminal client can run on a laptop or scanner-adjacent workstation. Platform packages are checksum-bound implementation details selected by the installer.

* **Low-resource execution** – CPU-only operation is the baseline. The tool processes one series at a time by default, caps memory use, checkpoints completed work and resumes safely after interruption. Faster machines may opt into bounded parallelism, but the privacy decisions and outputs remain identical.

* **Fail-closed upload** – Functional-EPI selection, DICOM metadata de-identification, privacy checks, and archive construction happen locally. Conversion and scientific validation happen asynchronously on the research cluster, against the exact received archive. Only an authorized series with a complete local privacy-pass record can enter the upload queue; unsupported or uncertain sequences remain on the source machine.

* **Measured sequence recognition** – Classification uses standard DICOM image/acquisition/temporal evidence plus exact scanner-family compatibility gates. Free-text sequence descriptions and vendor names alone are never sufficient. Scanner support is expanded only with non-PHI fixtures, recursive privacy tests, pixel equality, and conversion-equivalence evidence.

* **Security** – Control-plane and object transfer use encrypted connections. The workstation receives only short-lived, object- and checksum-scoped R2 upload capabilities; it never receives reusable cloud credentials. Processor jobs similarly receive short-lived object-scoped capabilities, while the canonical archive and derived outputs live in Cloudflare R2.

## **Legal, privacy and ethical considerations**

### **Informed consent and IRB approval**

* **Participant consent** – All scans shared through the pipeline must come from participants who have consented to share their de-identified data for research and open distribution. IRBs at each institution should review the consent language to ensure that automated sharing of unlabeled neuroimaging data is covered. This process is already working successfully at the Princeton Neuroscience Institute; we can provide relevant language from these consent/IRB forms as useful reference.

* **Jurisdiction variability** – Regulations differ across countries and states. In the U.S., HIPAA allows sharing of de‑identified health information; however, local IRBs may impose additional requirements. In Canada and the EU, data protection laws (e.g., Québec Law 25 or GDPR) can limit cross‑border transfer of health data. Collaborators must verify local policies. 

## **Benchmarking and evaluation**

* Related to dataset scaling, we additionally are interested in the development of meaningful benchmarks to evaluate foundation models on brain data. So far, fMRI foundation models have tested only a handful of trait-based tasks (age or gender prediction, control vs clinical group classification, etc.), and results were often saturated or lacked practical relevance. We propose the development of a standardized benchmark framework to evaluate neuroimaging foundation models. This direction likely necessitates its own separate research group from the present discussion surrounding scaling up dataset sharing.

## **Collaboration and outreach strategy**

### **Partnering with imaging centres and consortia**

1. **Leverage existing communities** – The Enigma consortia provide a network of hundreds of imaging centers worldwide. Each consortium focuses on a specific modality or clinical condition and maintains strong social connections among investigators. Engaging Enigma can quickly bootstrap adoption because many centres already collect data and have experience with multi‑site collaborations. Another key partner is OpenNeuro, which hosts tens of thousands of datasets and has preprocessed a large subset using fMRIPrep; partnering with OpenNeuro could provide infrastructure for storage, compute and discoverability. NeuroBagel focuses on discoverability and metadata and might also be a useful resource.

2. **Demonstrate value to contributors** – Facilities that contribute data gain access to the full aggregated dataset, advanced foundation models and compute resources. They can use the data for their own research, boosting their publications without incurring the cost of large‑scale data collection. Contributing also promotes open science and meets funders’ requirements for data sharing.

3. **Pilot sites** – Begin with a small number of pilot sites (e.g., the Norman Lab at Princeton, which already has a prototype pipeline). Use these pilots to refine the script, demonstrate reliability and produce a public example of successful automated sharing. Expand to additional sites gradually.

4. **Technical support** – Provide step‑by‑step installation guides and remote assistance. We can also consider providing monetary incentives to offset the initial work necessary to integrate our data sharing script and amend consent/IRB forms.  
5. **Grant collaborations** – Apply jointly for infrastructure grants (e.g., Brain Canada infrastructure call). Many such grants require matching funds and cross‑institutional partnerships. Foundations like Sloan, Arc or Simons may also support open‑science infrastructure.

## **Funding opportunities**

To sustain a petabyte‑scale neuroimaging resource, dedicated infrastructure funding is essential. Possible sources include:

* **Brain Canada Infrastructure grants** – These support large‑scale neuroinformatics infrastructure but require matching funds from partners. Collaboration with Canadian institutions (e.g., McGill) and philanthropic donors can help meet the match requirement.

* **U.S. National Science Foundation (NSF) Major Research Instrumentation (MRI) grants** – Provide funding for shared scientific infrastructure. Proposals must articulate community benefit and include multiple collaborating institutions.

* **Philanthropic foundations** – The Sloan Foundation, Arc Institute and Simons Foundation have funded open science and computational neuroscience projects. Their grants often favour projects that increase data accessibility and reproducibility.

* **Industry partnerships** – Cloud providers (Google Cloud, AWS, Microsoft Azure) may offer credits or cost‑sharing to support large datasets and compute. LightningAI and other AI companies may supply compute resources as part of corporate social responsibility programs.

## **Next steps**

Run the EPI-only terminal workflow with pilot laboratories; expand the fixture-backed scanner matrix; independently audit retained metadata and privacy on real authorized exports; build governed archive discovery/access and compatibility dashboards; and design the structural route separately around validated defacing and brain-preservation QC. In parallel, recruit pilot sites, develop IRB/consent guidance, and pursue infrastructure funding and institutional partnerships.
