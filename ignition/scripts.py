def read_bess_config(instance_path):
    """
    instance_path example: "[default]BESS/BESS_A"
    returns: (base_url, asset_id)
    """
    base_tag  = instance_path + "/Config/BaseUrl"
    asset_tag = instance_path + "/Config/AssetId"

    qvs = system.tag.readBlocking([base_tag, asset_tag])
    base_url = (qvs[0].value or "").strip()
    asset_id = (qvs[1].value or "").strip()

    if not base_url or not asset_id:
        raise ValueError("Missing BaseUrl or AssetId for %s" % instance_path)

    return base_url.rstrip("/") + "/" + asset_id.lstrip("/")
    
def read_asset(instance_path):
    """
    Poll the REST endpoint defined by instance_path/Config/BaseUrl + instance_path/Config/AssetId
    and write key fields into instance_path/telemetry/* tags.

    instance_path example: "[default]BESS/BESS_A"
    """
    client = system.net.httpClient(timeout=5000)  # connect timeout (ms)
    url = read_bess_config(instance_path)
	
    try:
    	logger = system.util.getLogger("timer.4sec")
#    	logger.info("tick at %s" % system.date.now())
        resp = client.get(url, timeout=10000, headers={"Accept": "application/json"})
        code = resp.getStatusCode()
        if code != 200:
            raise Exception("HTTP %s: %s" % (code, resp.getText()[:200]))

        data = system.util.jsonDecode(resp.getText())
        results = system.tag.browse("[default]BESS/")


        tag_paths = []
        values = []
        logger.trace("%s" % str(data))

        for tag in results.getResults():
            logger.trace(str(tag['name']))
            _results = system.tag.browse("[default]BESS/%s" % str(tag['name']))
            for _tag in _results.getResults():
            	tag_name = str(_tag['name'])
            	logger.trace(tag_name)
            	if tag_name in data:
            	    tag_paths.append("%s/%s" % (instance_path, tag_name))           
            	    values.append(data[tag_name])
            	
        # Map JSON keys -> telemetry tag relative paths
        mapping = {
            "soc_pct": "soc_pct",
            "soc_mwhr": "soc_mwhr",
            "current_mw": "current_mw",
            "setpoint_mw": "setpoint_mw",
            "min_mw": "min_mw",
            "max_mw": "max_mw",
            "status": "status",
            
        }
#        for json_key, tag_rel_path in mapping.items():
#        	logger.info("json_key: %s, tag_rel_path: %s" % (json_key, tag_rel_path))
#        	if json_key in data:
#        		tag_paths.append("%s/%s" % (instance_path, tag_rel_path))
#                values.append(data[json_key])

        # health / bookkeeping

        tag_paths += [
            "%s/commsOk" % instance_path,
            "%s/lastUpdateTs" % instance_path,
            "%s/lastError" % instance_path,
        ]
        values += [True, system.date.now(), ""]
#        for tp in tag_paths:
#        	logger.info("tagpath: %s" % tp)
        status = system.tag.writeBlocking(tag_paths, values)
        logger.debug("writeBlocking status: %s" % status)

    except Exception as e:
    	logger.error("Error writing to Ignition tags" % str(e))
        # mark comms bad + record error
        system.tag.writeBlocking(
            [
                "%s/commsOk" % instance_path,
                "%s/lastError" % instance_path,
                "%s/lastUpdateTs" % instance_path,
            ],
            [False, str(e), system.date.now()],
        )